//! RTSP/1.0 request parsing and response serialization (RFC 2326 sections 4 to 7).
//!
//! The parser is incremental and total: [`parse_incoming`] looks at the bytes buffered so far
//! and returns "need more", a complete message plus the bytes it consumed, or a typed error. It
//! never panics and never allocates more than the configured [`Limits`] allow.
//!
//! Interleaved `$` frames (RFC 2326 section 10.12) share the connection with requests, so the
//! entry point yields either.
//!
//! References:
//! - [RFC 2326 sections 4 to 7 (messages), 10.12 (interleaved binary data)](https://www.rfc-editor.org/rfc/rfc2326)

use std::fmt::Write as _;

use thiserror::Error;

/// The only protocol version this server speaks.
pub const VERSION: &str = "RTSP/1.0";

/// Size limits applied while parsing. Everything a peer can make us buffer is bounded here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum bytes in the request line plus headers, excluding the blank line.
    pub max_head_bytes: usize,
    /// Maximum number of header lines.
    pub max_headers: usize,
    /// Maximum `Content-Length`, and the maximum payload of an interleaved frame.
    pub max_body_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_head_bytes: 8 * 1024,
            max_headers: 64,
            max_body_bytes: 4 * 1024,
        }
    }
}

/// Why a message could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    /// The bytes violate the RTSP grammar.
    #[error("malformed request: {0}")]
    Malformed(&'static str),
    /// The request line and headers exceed [`Limits::max_head_bytes`].
    #[error("request head exceeds {limit} bytes")]
    HeadTooLarge {
        /// The configured limit.
        limit: usize,
    },
    /// More than [`Limits::max_headers`] header lines.
    #[error("more than {limit} headers")]
    TooManyHeaders {
        /// The configured limit.
        limit: usize,
    },
    /// A body or interleaved frame larger than [`Limits::max_body_bytes`].
    #[error("body of {len} bytes exceeds limit of {limit}")]
    BodyTooLarge {
        /// Declared length.
        len: usize,
        /// The configured limit.
        limit: usize,
    },
    /// The request line names a protocol version other than `RTSP/1.0`.
    #[error("unsupported RTSP version")]
    UnsupportedVersion,
}

impl ParseError {
    /// The RTSP status code to answer with before closing the connection.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::Malformed(_) => 400,
            Self::HeadTooLarge { .. } | Self::TooManyHeaders { .. } | Self::BodyTooLarge { .. } => {
                413
            }
            Self::UnsupportedVersion => 505,
        }
    }
}

/// An RTSP request method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// `OPTIONS`
    Options,
    /// `DESCRIBE`
    Describe,
    /// `SETUP`
    Setup,
    /// `PLAY`
    Play,
    /// `TEARDOWN`
    Teardown,
    /// `GET_PARAMETER`
    GetParameter,
    /// Any other well-formed method token.
    Other(String),
}

impl Method {
    fn from_token(token: &str) -> Self {
        match token {
            "OPTIONS" => Self::Options,
            "DESCRIBE" => Self::Describe,
            "SETUP" => Self::Setup,
            "PLAY" => Self::Play,
            "TEARDOWN" => Self::Teardown,
            "GET_PARAMETER" => Self::GetParameter,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The method as it appears on the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Options => "OPTIONS",
            Self::Describe => "DESCRIBE",
            Self::Setup => "SETUP",
            Self::Play => "PLAY",
            Self::Teardown => "TEARDOWN",
            Self::GetParameter => "GET_PARAMETER",
            Self::Other(token) => token,
        }
    }
}

/// An ordered, case-insensitive header map. Duplicate names are kept.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Headers(Vec<(String, String)>);

impl std::fmt::Debug for Headers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Headers")
            .field("values", &"<redacted>")
            .finish()
    }
}

impl Headers {
    /// The first value for `name`.
    #[must_use]
    pub fn get<'a>(&'a self, name: &'a str) -> Option<&'a str> {
        self.get_all(name).next()
    }

    /// Every value for `name`, in wire order.
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.0
            .iter()
            .filter(move |(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Append a header. Control characters are stripped from both parts so a value can never
    /// inject a line break into a serialized message.
    pub fn push(&mut self, name: &str, value: &str) {
        let clean = |s: &str, extra: &[char]| -> String {
            s.chars()
                .filter(|c| !c.is_control() && !extra.contains(c))
                .collect()
        };
        self.0.push((clean(name, &[':', ' ']), clean(value, &[])));
    }

    /// Iterate `(name, value)` pairs in wire order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    /// Number of headers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are no headers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A parsed RTSP request.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    /// The request method.
    pub method: Method,
    /// The request URI exactly as sent (needed verbatim for Digest `uri` comparison).
    pub uri: String,
    /// Header lines.
    pub headers: Headers,
    /// Body bytes, `Content-Length` long.
    pub body: Vec<u8>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("uri", &"[REDACTED]")
            .field("headers", &self.headers)
            .field("body_len", &self.body.len())
            .finish()
    }
}

impl Request {
    /// Serialize onto `out` with `CSeq` and `Content-Length` added.
    pub fn write_into(&self, cseq: u32, out: &mut Vec<u8>) {
        let mut head = String::new();
        // Writing to a `String` cannot fail.
        let _ = write!(
            head,
            "{} {} {VERSION}\r\nCSeq: {cseq}\r\n",
            self.method.as_str(),
            self.uri
        );
        for (name, value) in self.headers.iter().filter(|(n, _)| !is_framing(n)) {
            let _ = write!(head, "{name}: {value}\r\n");
        }
        let _ = write!(head, "Content-Length: {}\r\n\r\n", self.body.len());
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(&self.body);
    }

    /// The `CSeq` header as a number, if present and valid.
    #[must_use]
    pub fn cseq(&self) -> Option<u32> {
        self.headers.get("cseq")?.trim().parse().ok()
    }
}

/// What arrived on the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// An RTSP request.
    Request(Request),
    /// An interleaved binary frame (`$`, channel, length, payload).
    Interleaved {
        /// Channel identifier.
        channel: u8,
        /// Frame payload.
        payload: Vec<u8>,
    },
}

/// Try to parse one message from the front of `buf`.
///
/// Returns `Ok(None)` when more bytes are needed, otherwise the message and the number of bytes
/// it occupied.
///
/// # Errors
///
/// A [`ParseError`] for anything malformed or over `limits`. After an error the stream cannot
/// be resynchronised and the connection should be closed.
pub fn parse_incoming(
    buf: &[u8], limits: &Limits,
) -> Result<Option<(Incoming, usize)>, ParseError> {
    if buf.first() == Some(&b'$') {
        return parse_frame(buf, limits);
    }
    parse_request(buf, limits)
}

fn parse_frame(buf: &[u8], limits: &Limits) -> Result<Option<(Incoming, usize)>, ParseError> {
    let (Some(&channel), Some(&hi), Some(&lo)) = (buf.get(1), buf.get(2), buf.get(3)) else {
        return Ok(None);
    };
    let len = usize::from(u16::from_be_bytes([hi, lo]));
    if len > limits.max_body_bytes {
        return Err(ParseError::BodyTooLarge {
            len,
            limit: limits.max_body_bytes,
        });
    }
    let end = 4 + len;
    let Some(payload) = buf.get(4..end) else {
        return Ok(None);
    };
    Ok(Some((
        Incoming::Interleaved {
            channel,
            payload: payload.to_vec(),
        },
        end,
    )))
}

/// A parsed start line, headers, body and the total bytes consumed.
type Parsed<T> = (T, Headers, Vec<u8>, usize);

/// Shared framing for requests and responses: find the head, parse `start` and the headers,
/// then take `Content-Length` body bytes.
fn parse_message<T>(
    buf: &[u8], limits: &Limits, start: impl FnOnce(&str) -> Result<T, ParseError>,
) -> Result<Option<Parsed<T>>, ParseError> {
    const END: &[u8] = b"\r\n\r\n";
    let window = buf.get(
        ..buf
            .len()
            .min(limits.max_head_bytes.saturating_add(END.len())),
    );
    let head_len = window.and_then(|w| w.windows(END.len()).position(|p| p == END));
    let Some(head_len) = head_len else {
        // A head of exactly `max_head_bytes` may still be waiting for up to `END.len() - 1` bytes
        // of its terminator, so only a full extra delimiter's worth proves the head too long.
        return if buf.len() >= limits.max_head_bytes.saturating_add(END.len()) {
            Err(ParseError::HeadTooLarge {
                limit: limits.max_head_bytes,
            })
        } else {
            Ok(None)
        };
    };
    if head_len > limits.max_head_bytes {
        return Err(ParseError::HeadTooLarge {
            limit: limits.max_head_bytes,
        });
    }
    let head = buf
        .get(..head_len)
        .and_then(|h| std::str::from_utf8(h).ok())
        .ok_or(ParseError::Malformed("head is not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let first = lines.next().ok_or(ParseError::Malformed("empty message"))?;
    let start = start(first)?;
    let headers = parse_headers(lines, limits)?;

    let body_len = content_length(&headers, limits)?;
    let body_start = head_len + END.len();
    let end = body_start.saturating_add(body_len);
    let Some(body) = buf.get(body_start..end) else {
        return Ok(None);
    };
    Ok(Some((start, headers, body.to_vec(), end)))
}

fn parse_request(buf: &[u8], limits: &Limits) -> Result<Option<(Incoming, usize)>, ParseError> {
    Ok(parse_message(buf, limits, parse_request_line)?.map(
        |((method, uri), headers, body, used)| {
            (
                Incoming::Request(Request {
                    method,
                    uri,
                    headers,
                    body,
                }),
                used,
            )
        },
    ))
}

/// Try to parse one response from the front of `buf` (for clients and tests).
///
/// Same contract as [`parse_incoming`]: `Ok(None)` means more bytes are needed.
///
/// # Errors
///
/// A [`ParseError`] for anything malformed or over `limits`.
pub fn parse_response(
    buf: &[u8], limits: &Limits,
) -> Result<Option<(Response, usize)>, ParseError> {
    Ok(
        parse_message(buf, limits, parse_status_line)?.map(|(status, headers, body, used)| {
            (
                Response {
                    status,
                    headers,
                    body,
                },
                used,
            )
        }),
    )
}

/// Headers the serializers own: stale copies from a parsed message must not be re-emitted.
fn is_framing(name: &str) -> bool {
    name.eq_ignore_ascii_case("cseq") || name.eq_ignore_ascii_case("content-length")
}

fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn check_version(version: &str) -> Result<(), ParseError> {
    if version == VERSION {
        Ok(())
    } else if version.starts_with("RTSP/") {
        Err(ParseError::UnsupportedVersion)
    } else {
        Err(ParseError::Malformed("bad protocol version"))
    }
}

fn parse_request_line(line: &str) -> Result<(Method, String), ParseError> {
    let mut parts = line.split(' ');
    let (Some(method), Some(uri), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ParseError::Malformed("request line needs three fields"));
    };
    if !is_token(method) {
        return Err(ParseError::Malformed("method is not a token"));
    }
    if uri.is_empty() || uri.chars().any(char::is_control) {
        return Err(ParseError::Malformed("bad request URI"));
    }
    check_version(version)?;
    Ok((Method::from_token(method), uri.to_owned()))
}

fn parse_status_line(line: &str) -> Result<u16, ParseError> {
    let mut parts = line.splitn(3, ' ');
    let (Some(version), Some(code)) = (parts.next(), parts.next()) else {
        return Err(ParseError::Malformed(
            "status line needs a version and code",
        ));
    };
    check_version(version)?;
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ParseError::Malformed("bad status code"));
    }
    code.parse()
        .map_err(|_| ParseError::Malformed("bad status code"))
}

fn parse_headers<'a>(
    lines: impl Iterator<Item = &'a str>, limits: &Limits,
) -> Result<Headers, ParseError> {
    let mut headers = Headers::default();
    for line in lines {
        if headers.len() >= limits.max_headers {
            return Err(ParseError::TooManyHeaders {
                limit: limits.max_headers,
            });
        }
        if line.starts_with([' ', '\t']) {
            return Err(ParseError::Malformed("folded headers are not supported"));
        }
        if line.contains(['\r', '\n']) {
            return Err(ParseError::Malformed("bare line break in header"));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(ParseError::Malformed("header without colon"))?;
        if !is_token(name) {
            return Err(ParseError::Malformed("header name is not a token"));
        }
        headers.push(name, value.trim_matches([' ', '\t']));
    }
    Ok(headers)
}

fn content_length(headers: &Headers, limits: &Limits) -> Result<usize, ParseError> {
    let mut length: Option<usize> = None;
    for value in headers.get_all("content-length") {
        let value = value.trim();
        if value.is_empty() || value.len() > 9 || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ParseError::Malformed("bad Content-Length"));
        }
        let parsed: usize = value
            .parse()
            .map_err(|_| ParseError::Malformed("bad Content-Length"))?;
        if length.is_some_and(|prev| prev != parsed) {
            return Err(ParseError::Malformed("conflicting Content-Length"));
        }
        length = Some(parsed);
    }
    let length = length.unwrap_or(0);
    if length > limits.max_body_bytes {
        return Err(ParseError::BodyTooLarge {
            len: length,
            limit: limits.max_body_bytes,
        });
    }
    Ok(length)
}

/// An RTSP response under construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// Numeric status code.
    pub status: u16,
    /// Header lines (`CSeq` and `Content-Length` are added on serialization).
    pub headers: Headers,
    /// Body bytes.
    pub body: Vec<u8>,
}

impl Response {
    /// A response with no headers and no body.
    #[must_use]
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: Headers::default(),
            body: Vec::new(),
        }
    }

    /// Add a header.
    #[must_use]
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push(name, value);
        self
    }

    /// Set the body and its `Content-Type`.
    #[must_use]
    pub fn with_body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers.push("Content-Type", content_type);
        self.body = body;
        self
    }

    /// Serialize onto `out`, echoing `cseq` when known. `Content-Length` is always written.
    pub fn write_into(&self, cseq: Option<u32>, out: &mut Vec<u8>) {
        let mut head = String::new();
        // Writing to a `String` cannot fail.
        let _ = write!(
            head,
            "{VERSION} {} {}\r\n",
            self.status,
            reason_phrase(self.status)
        );
        if let Some(cseq) = cseq {
            let _ = write!(head, "CSeq: {cseq}\r\n");
        }
        for (name, value) in self.headers.iter().filter(|(n, _)| !is_framing(n)) {
            let _ = write!(head, "{name}: {value}\r\n");
        }
        let _ = write!(head, "Content-Length: {}\r\n\r\n", self.body.len());
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(&self.body);
    }
}

/// The reason phrase for a status code (RFC 2326 section 7.1.1).
#[must_use]
pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Request Entity Too Large",
        451 => "Parameter Not Understood",
        454 => "Session Not Found",
        455 => "Method Not Valid In This State",
        461 => "Unsupported Transport",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        505 => "RTSP Version Not Supported",
        _ => "Unknown",
    }
}

/// Error from [`encode_interleaved`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("interleaved payload of {0} bytes does not fit a 16-bit length")]
pub struct FrameTooLarge(pub usize);

/// Append an interleaved frame (`$`, channel, big-endian length, payload) to `out`.
///
/// # Errors
///
/// [`FrameTooLarge`] when `payload` is longer than 65535 bytes. `out` is untouched then.
pub fn encode_interleaved(
    channel: u8, payload: &[u8], out: &mut Vec<u8>,
) -> Result<(), FrameTooLarge> {
    let len = u16::try_from(payload.len()).map_err(|_| FrameTooLarge(payload.len()))?;
    out.push(b'$');
    out.push(channel);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn parsed_request_uri_credentials_are_redacted() {
        let bytes =
            b"OPTIONS rtsp://synthetic-user:synthetic-password@192.0.2.1/stream RTSP/1.0\r\n\r\n";
        let (incoming, _) = super::parse_incoming(bytes, &super::Limits::default())
            .unwrap()
            .unwrap();
        let request = request(bytes);
        for debug in [format!("{incoming:?}"), format!("{request:?}")] {
            assert!(!debug.contains("synthetic-user"));
            assert!(!debug.contains("synthetic-password"));
        }
    }

    #[test]
    fn parsed_request_body_is_redacted() {
        let body = b"synthetic-secret";
        let (incoming, _) = super::parse_incoming(
            b"OPTIONS / RTSP/1.0\r\nContent-Length: 16\r\n\r\nsynthetic-secret",
            &super::Limits::default(),
        )
        .unwrap()
        .unwrap();
        let request = request(b"OPTIONS / RTSP/1.0\r\nContent-Length: 16\r\n\r\nsynthetic-secret");
        for debug in [format!("{incoming:?}"), format!("{request:?}")] {
            assert!(!debug.contains("synthetic-secret"));
            assert!(!debug.contains(&format!("{:?}", body.as_slice())));
            assert!(debug.contains("body_len: 16"));
        }
    }

    #[test]
    fn parsed_debug_redacts_authorization() {
        let (incoming, _) = super::parse_incoming(b"OPTIONS rtsp://example.test/stream RTSP/1.0\r\nAuthorization: synthetic-secret\r\n\r\n", &super::Limits::default()).unwrap().unwrap();
        assert!(!format!("{incoming:?}").contains("synthetic-secret"));
        let parsed = request(b"OPTIONS rtsp://example.test/stream RTSP/1.0\r\nAuthorization: synthetic-secret\r\n\r\n");
        assert!(!format!("{parsed:?}").contains("synthetic-secret"));
        assert!(!format!("{:?}", parsed.headers).contains("synthetic-secret"));
    }
    use proptest::prelude::*;

    use super::*;

    const LIMITS: Limits = Limits {
        max_head_bytes: 256,
        max_headers: 4,
        max_body_bytes: 16,
    };

    fn request(buf: &[u8]) -> Request {
        match parse_incoming(buf, &LIMITS) {
            Ok(Some((Incoming::Request(r), n))) => {
                assert_eq!(n, buf.len());
                r
            }
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn parses_options() {
        let r = request(
            b"OPTIONS rtsp://192.0.2.1/stream RTSP/1.0\r\nCSeq: 7\r\nUser-Agent: t\r\n\r\n",
        );
        assert_eq!(r.method, Method::Options);
        assert_eq!(r.uri, "rtsp://192.0.2.1/stream");
        assert_eq!(r.cseq(), Some(7));
        assert_eq!(r.headers.get("user-agent"), Some("t"));
    }

    #[test]
    fn parses_body_and_reports_consumed() {
        let wire = b"GET_PARAMETER * RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 3\r\n\r\nabcOPTIONS";
        let (msg, n) = parse_incoming(wire, &LIMITS).unwrap().unwrap();
        let Incoming::Request(r) = msg else { panic!() };
        assert_eq!(r.method, Method::GetParameter);
        assert_eq!(r.body, b"abc");
        assert_eq!(&wire[n..], b"OPTIONS");
    }

    #[test]
    fn incomplete_input_asks_for_more() {
        for wire in [
            &b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n"[..],
            b"OPTIONS * RTSP/1.0\r\nContent-Length: 4\r\n\r\nab",
            b"$\x00\x00",
            b"$\x00\x00\x05ab",
            b"",
        ] {
            assert_eq!(parse_incoming(wire, &LIMITS), Ok(None), "{wire:?}");
        }
    }

    #[test]
    fn parses_interleaved_frame() {
        let mut wire = Vec::new();
        encode_interleaved(3, b"hello", &mut wire).unwrap();
        assert_eq!(
            parse_incoming(&wire, &LIMITS),
            Ok(Some((
                Incoming::Interleaved {
                    channel: 3,
                    payload: b"hello".to_vec()
                },
                9
            )))
        );
    }

    #[test]
    fn rejects_oversize_frame_and_encode_overflow() {
        assert!(matches!(
            parse_incoming(b"$\x00\x00\x11", &LIMITS),
            Err(ParseError::BodyTooLarge { len: 17, limit: 16 })
        ));
        let mut out = Vec::new();
        assert_eq!(
            encode_interleaved(0, &vec![0; 70_000], &mut out),
            Err(FrameTooLarge(70_000))
        );
        assert_eq!(out, Vec::<u8>::new());
    }

    #[test]
    fn rejects_malformed_requests() {
        let cases: &[&[u8]] = &[
            b"OPTIONS * RTSP/1.0 extra\r\n\r\n",
            b"OPTIONS *\r\n\r\n",
            b"OP TIONS * RTSP/1.0\r\n\r\n",
            b"OPTIONS * HTTP/1.1\r\n\r\n",
            b"OPTIONS * RTSP/1.0\r\nno colon\r\n\r\n",
            b"OPTIONS * RTSP/1.0\r\n folded: x\r\n\r\n",
            b"OPTIONS * RTSP/1.0\r\nbad name: x\r\n\r\n",
            b"OPTIONS * RTSP/1.0\r\nContent-Length: -1\r\n\r\n",
            b"OPTIONS * RTSP/1.0\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            b"OPTIONS * RTSP/1.0\nCSeq: 1\r\n\r\n",
            b"OPTIONS * RTSP/1.0\r\nX: \xff\r\n\r\n",
        ];
        for wire in cases {
            assert!(
                matches!(parse_incoming(wire, &LIMITS), Err(ParseError::Malformed(_))),
                "{wire:?}"
            );
        }
    }

    #[test]
    fn enforces_limits() {
        assert_eq!(
            parse_incoming(b"OPTIONS * RTSP/2.0\r\n\r\n", &LIMITS),
            Err(ParseError::UnsupportedVersion)
        );
        let many = b"OPTIONS * RTSP/1.0\r\na: 1\r\nb: 2\r\nc: 3\r\nd: 4\r\ne: 5\r\n\r\n";
        assert_eq!(
            parse_incoming(many, &LIMITS),
            Err(ParseError::TooManyHeaders { limit: 4 })
        );
        let mut long = b"OPTIONS * RTSP/1.0\r\nX: ".to_vec();
        long.extend(std::iter::repeat_n(b'a', 300));
        // Without a terminator and with a terminator: both are rejected.
        assert!(matches!(
            parse_incoming(&long, &LIMITS),
            Err(ParseError::HeadTooLarge { .. })
        ));
        long.extend_from_slice(b"\r\n\r\n");
        assert!(matches!(
            parse_incoming(&long, &LIMITS),
            Err(ParseError::HeadTooLarge { .. })
        ));
        assert!(matches!(
            parse_incoming(b"OPTIONS * RTSP/1.0\r\nContent-Length: 17\r\n\r\n", &LIMITS),
            Err(ParseError::BodyTooLarge { len: 17, limit: 16 })
        ));
    }

    #[test]
    fn response_round_trips_through_parse_response() {
        let mut wire = Vec::new();
        Response::new(461)
            .with_header("Session", "abc;timeout=60")
            .with_body("application/sdp", b"v=0\r\n".to_vec())
            .write_into(Some(12), &mut wire);
        let used = wire.len();
        wire.extend_from_slice(b"$\x00\x00\x00");
        let (reply, n) = parse_response(&wire, &LIMITS).unwrap().unwrap();
        assert_eq!(n, used);
        assert_eq!(reply.status, 461);
        assert_eq!(reply.body, b"v=0\r\n");
        assert_eq!(reply.headers.get("cseq"), Some("12"));
        assert_eq!(reply.headers.get("session"), Some("abc;timeout=60"));
        assert_eq!(parse_response(&wire[..used - 1], &LIMITS), Ok(None));
    }

    #[test]
    fn request_round_trips_through_parse_incoming() {
        let mut headers = Headers::default();
        headers.push("Session", "abc");
        let original = Request {
            method: Method::GetParameter,
            uri: "rtsp://192.0.2.1/stream".into(),
            headers,
            body: b"hi".to_vec(),
        };
        let mut wire = Vec::new();
        original.write_into(5, &mut wire);
        let Ok(Some((Incoming::Request(parsed), n))) = parse_incoming(&wire, &LIMITS) else {
            panic!("did not parse");
        };
        assert_eq!(n, wire.len());
        assert_eq!(parsed.method, original.method);
        assert_eq!(parsed.uri, original.uri);
        assert_eq!(parsed.body, original.body);
        assert_eq!(parsed.cseq(), Some(5));
        assert_eq!(parsed.headers.get("session"), Some("abc"));
    }

    #[test]
    fn reserializing_parsed_messages_keeps_framing_authoritative() {
        let wire = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 0\r\nX: y\r\n\r\n";
        let (mut reply, _) = parse_response(wire, &LIMITS).unwrap().unwrap();
        reply.body = b"abc".to_vec();
        let mut out = Vec::new();
        reply.write_into(Some(9), &mut out);
        let (again, n) = parse_response(&out, &LIMITS).unwrap().unwrap();
        assert_eq!(n, out.len());
        assert_eq!(again.body, b"abc");
        assert_eq!(again.headers.get_all("cseq").collect::<Vec<_>>(), ["9"]);
        assert_eq!(again.headers.get("x"), Some("y"));

        let req_wire = b"GET_PARAMETER * RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 0\r\n\r\n";
        let Ok(Some((Incoming::Request(mut req), _))) = parse_incoming(req_wire, &LIMITS) else {
            panic!("did not parse");
        };
        req.body = b"xy".to_vec();
        let mut out = Vec::new();
        req.write_into(4, &mut out);
        let Ok(Some((Incoming::Request(again), n))) = parse_incoming(&out, &LIMITS) else {
            panic!("did not reparse");
        };
        assert_eq!(
            (n, again.cseq(), again.body),
            (out.len(), Some(4), b"xy".to_vec())
        );
    }

    #[test]
    fn rejects_malformed_responses() {
        for wire in [
            &b"HTTP/1.1 200 OK\r\n\r\n"[..],
            b"RTSP/1.0\r\n\r\n",
            b"RTSP/1.0 2x0 OK\r\n\r\n",
            b"RTSP/1.0 20 OK\r\n\r\n",
        ] {
            assert!(
                matches!(parse_response(wire, &LIMITS), Err(ParseError::Malformed(_))),
                "{wire:?}"
            );
        }
        assert_eq!(
            parse_response(b"RTSP/1.0 200\r\nCSeq: 1\r\n\r\n", &LIMITS)
                .map(|r| r.map(|(r, _)| r.status)),
            Ok(Some(200))
        );
    }

    #[test]
    fn head_at_the_limit_survives_every_split_of_the_delimiter() {
        let limits = Limits {
            max_head_bytes: 64,
            ..LIMITS
        };
        let prefix = "OPTIONS * RTSP/1.0\r\nX: ";
        let pad = "a".repeat(64 - prefix.len());
        let exact = format!("{prefix}{pad}\r\n\r\n").into_bytes();
        assert_eq!(exact.len(), 64 + 4);
        for cut in 0..exact.len() {
            assert_eq!(
                parse_incoming(&exact[..cut], &limits),
                Ok(None),
                "cut {cut}"
            );
        }
        assert!(matches!(parse_incoming(&exact, &limits), Ok(Some(_))));
        // One byte more is over the limit however the terminator arrives.
        let over = format!("{prefix}{pad}a\r\n\r\n").into_bytes();
        for cut in 0..over.len() {
            assert!(
                matches!(
                    parse_incoming(&over[..cut], &limits),
                    Ok(None) | Err(ParseError::HeadTooLarge { .. })
                ),
                "cut {cut}"
            );
        }
        assert!(matches!(
            parse_incoming(&over, &limits),
            Err(ParseError::HeadTooLarge { .. })
        ));
        assert!(matches!(
            parse_incoming(&over[..over.len() - 1], &limits),
            Err(ParseError::HeadTooLarge { .. })
        ));
    }

    #[test]
    fn extreme_limits_do_not_overflow() {
        let huge = Limits {
            max_head_bytes: usize::MAX,
            max_headers: usize::MAX,
            max_body_bytes: usize::MAX,
        };
        assert_eq!(parse_incoming(b"", &huge), Ok(None));
        assert_eq!(parse_incoming(b"OPTIONS * RTSP/1.0\r\n", &huge), Ok(None));
        let wire = b"OPTIONS * RTSP/1.0\r\nContent-Length: 999999999\r\n\r\n";
        assert_eq!(parse_incoming(wire, &huge), Ok(None));
    }

    #[test]
    fn error_status_codes() {
        assert_eq!(ParseError::Malformed("x").status(), 400);
        assert_eq!(ParseError::HeadTooLarge { limit: 1 }.status(), 413);
        assert_eq!(ParseError::UnsupportedVersion.status(), 505);
    }

    #[test]
    fn serializes_response() {
        let mut out = Vec::new();
        Response::new(200)
            .with_header("Public", "OPTIONS")
            .with_body("application/sdp", b"v=0".to_vec())
            .write_into(Some(9), &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "RTSP/1.0 200 OK\r\nCSeq: 9\r\nPublic: OPTIONS\r\nContent-Type: application/sdp\r\nContent-Length: 3\r\n\r\nv=0"
        );
    }

    #[test]
    fn header_values_cannot_inject_lines() {
        let mut out = Vec::new();
        Response::new(404)
            .with_header("X-Evil", "a\r\nInjected: 1")
            .write_into(None, &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("\r\nInjected"));
        assert!(text.starts_with("RTSP/1.0 404 Not Found\r\n"));
    }

    #[test]
    fn headers_are_case_insensitive() {
        let mut h = Headers::default();
        h.push("Session", "a");
        h.push("SESSION", "b");
        assert_eq!(h.get("session"), Some("a"));
        assert_eq!(h.get_all("Session").collect::<Vec<_>>(), ["a", "b"]);
        assert!(!h.is_empty());
    }

    proptest! {
        #[test]
        fn parser_never_panics_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let _ = parse_incoming(&bytes, &Limits::default());
            let _ = parse_response(&bytes, &Limits::default());
        }

        #[test]
        fn parser_never_panics_on_request_shaped_bytes(
            method in "[A-Z_]{0,12}",
            uri in "[ -~]{0,40}",
            tail in proptest::collection::vec(any::<u8>(), 0..256),
        ) {
            let mut wire = format!("{method} {uri} RTSP/1.0\r\n").into_bytes();
            wire.extend_from_slice(&tail);
            wire.extend_from_slice(b"\r\n\r\n");
            let _ = parse_incoming(&wire, &Limits::default());
        }

        #[test]
        fn every_prefix_of_a_valid_request_is_incomplete_or_ok(cut in 0usize..80) {
            let wire = b"SETUP rtsp://192.0.2.1/s/t RTSP/1.0\r\nCSeq: 3\r\nContent-Length: 2\r\n\r\nhi";
            let cut = cut.min(wire.len());
            let got = parse_incoming(&wire[..cut], &Limits::default()).unwrap();
            prop_assert_eq!(got.is_some(), cut == wire.len());
        }
    }
}
