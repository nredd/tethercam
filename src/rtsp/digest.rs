//! HTTP Digest authentication for RTSP (RFC 7616, `qop=auth` only).
//!
//! Policy:
//! - Required on every request, `OPTIONS` included, loopback included. No Basic
//! - `SHA-256` is challenged first, `MD5` second, because common clients only speak `MD5`
//! - The server remembers every nonce it issues with an expiry and the highest `nc` seen.
//!   A repeated or lower `nc` is a replay and is rejected
//! - RFC 2069 compatibility: a `qop`-less `MD5` response (`MD5(HA1:nonce:HA2)`, what live555
//!   based clients send even when `qop="auth"` is offered) is accepted. `SHA-256` always
//!   needs `qop`. Such a response carries no `nc`, so replay is bounded differently: its nonce
//!   is valid only on the control connection it was issued on and only until it expires. A
//!   captured header therefore cannot be replayed by anyone who does not already own that TCP
//!   connection. Within the connection and lifetime it is reusable, as RFC 2069 intends
//! - A correct digest over an expired or unknown nonce yields `stale=true` so the client retries
//!   without prompting. A wrong digest never does
//! - Only the password hashes are kept, and credential comparison is constant time
//!
//! References:
//! - [RFC 7616](https://www.rfc-editor.org/rfc/rfc7616)
//! - [RFC 2326 section 14.5 (`Authorization`)](https://www.rfc-editor.org/rfc/rfc2326#section-14.5)

use std::collections::HashMap;
use std::fmt;
use std::hint::black_box;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use md5::Md5;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Upper bound on the `Authorization` header we are willing to parse.
const MAX_HEADER_LEN: usize = 1024;
/// Upper bound on the client nonce length.
const MAX_CNONCE_LEN: usize = 128;
/// Upper bound on outstanding nonces; the oldest is evicted past this.
const DEFAULT_MAX_NONCES: usize = 1024;

/// Errors from setting up or running authentication.
#[derive(Debug, Error)]
pub enum DigestError {
    /// The username, password or realm cannot be used in a Digest exchange.
    #[error("invalid credential field: {0}")]
    InvalidCredentials(&'static str),
    /// The operating system CSPRNG failed.
    #[error("random number generation failed: {0}")]
    Random(getrandom::Error),
}

/// Identifies the control connection a nonce was issued on.
pub type ConnectionId = u64;

/// A hash algorithm offered in the challenge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    /// `SHA-256`
    Sha256,
    /// `MD5`, for client compatibility
    Md5,
}

impl Algorithm {
    /// The token used in `algorithm=`.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Sha256 => "SHA-256",
            Self::Md5 => "MD5",
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        if token.eq_ignore_ascii_case("SHA-256") {
            Some(Self::Sha256)
        } else if token.eq_ignore_ascii_case("MD5") {
            Some(Self::Md5)
        } else {
            None
        }
    }

    /// Lowercase hex digest of the concatenation of `parts`.
    #[must_use]
    pub fn hash_hex(self, parts: &[&[u8]]) -> String {
        match self {
            Self::Sha256 => {
                let mut h = Sha256::new();
                for part in parts {
                    h.update(part);
                }
                hex(&h.finalize())
            }
            Self::Md5 => {
                let mut h = Md5::new();
                for part in parts {
                    h.update(part);
                }
                hex(&h.finalize())
            }
        }
    }
}

pub(super) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            // Writing to a `String` cannot fail.
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Constant-time equality for equal-length inputs. Length is not secret (digests are fixed
/// width) so differing lengths return early.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y));
    black_box(diff) == 0
}

/// The single account the server authenticates. Holds `H(user:realm:password)` for both
/// algorithms and never the password itself.
#[derive(Clone)]
pub struct Credentials {
    username: String,
    realm: String,
    ha1_sha256: String,
    ha1_md5: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("realm", &self.realm)
            .field("secret", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// Derive credentials for `realm`.
    ///
    /// # Errors
    ///
    /// [`DigestError::InvalidCredentials`] when `username` or `password` is empty, `username`
    /// contains `:`, or any field has control characters, quotes or backslashes.
    pub fn new(username: &str, password: &str, realm: &str) -> Result<Self, DigestError> {
        let bad = |s: &str| s.chars().any(|c| c.is_control() || c == '"' || c == '\\');
        if username.is_empty() || username.contains(':') || bad(username) {
            return Err(DigestError::InvalidCredentials("username"));
        }
        if password.is_empty() || password.chars().any(char::is_control) {
            return Err(DigestError::InvalidCredentials("password"));
        }
        if realm.is_empty() || bad(realm) {
            return Err(DigestError::InvalidCredentials("realm"));
        }
        let base = format!("{username}:{realm}:{password}");
        Ok(Self {
            username: username.to_owned(),
            realm: realm.to_owned(),
            ha1_sha256: Algorithm::Sha256.hash_hex(&[base.as_bytes()]),
            ha1_md5: Algorithm::Md5.hash_hex(&[base.as_bytes()]),
        })
    }

    fn ha1(&self, algorithm: Algorithm) -> &str {
        match algorithm {
            Algorithm::Sha256 => &self.ha1_sha256,
            Algorithm::Md5 => &self.ha1_md5,
        }
    }
}

/// The result of checking one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthOutcome {
    /// Credentials verified; serve the request.
    Authorized,
    /// Answer `401` with a fresh challenge.
    Challenge {
        /// Set `stale=true`: the digest was right but the nonce was not usable.
        stale: bool,
        /// A digest was presented and failed (as opposed to none being presented).
        failed_attempt: bool,
    },
}

const fn challenge(stale: bool, failed_attempt: bool) -> AuthOutcome {
    AuthOutcome::Challenge {
        stale,
        failed_attempt,
    }
}

#[derive(Debug)]
struct NonceState {
    issued: Instant,
    conn: ConnectionId,
    last_nc: u32,
}

/// Issues challenges and verifies `Authorization` headers.
pub struct Authenticator {
    credentials: Credentials,
    nonce_lifetime: Duration,
    max_nonces: usize,
    nonces: Mutex<HashMap<String, NonceState>>,
}

impl fmt::Debug for Authenticator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authenticator")
            .field("credentials", &self.credentials)
            .field("nonce_lifetime", &self.nonce_lifetime)
            .field("max_nonces", &self.max_nonces)
            .field("nonces", &"[REDACTED]")
            .finish()
    }
}

impl Authenticator {
    /// An authenticator whose nonces live for `nonce_lifetime`.
    #[must_use]
    pub fn new(credentials: Credentials, nonce_lifetime: Duration) -> Self {
        Self::with_nonce_capacity(credentials, nonce_lifetime, DEFAULT_MAX_NONCES)
    }

    /// Like [`Authenticator::new`] with an explicit bound on outstanding nonces.
    #[must_use]
    pub fn with_nonce_capacity(
        credentials: Credentials, nonce_lifetime: Duration, max_nonces: usize,
    ) -> Self {
        Self {
            credentials,
            nonce_lifetime,
            max_nonces: max_nonces.max(1),
            nonces: Mutex::new(HashMap::new()),
        }
    }

    /// Issue a fresh nonce for `conn` and the `WWW-Authenticate` header values (`SHA-256`, then `MD5`).
    ///
    /// # Errors
    ///
    /// [`DigestError::Random`] if the CSPRNG fails.
    pub fn challenge(
        &self, conn: ConnectionId, stale: bool, now: Instant,
    ) -> Result<Vec<String>, DigestError> {
        let mut raw = [0_u8; 16];
        getrandom::fill(&mut raw).map_err(DigestError::Random)?;
        let nonce = hex(&raw);
        {
            let mut nonces = self.nonces.lock().unwrap_or_else(PoisonError::into_inner);
            nonces.retain(|_, s| now.saturating_duration_since(s.issued) < self.nonce_lifetime);
            while nonces.len() >= self.max_nonces {
                let oldest = nonces
                    .iter()
                    .min_by_key(|(_, s)| s.issued)
                    .map(|(k, _)| k.clone());
                match oldest {
                    Some(k) => nonces.remove(&k),
                    None => break,
                };
            }
            nonces.insert(
                nonce.clone(),
                NonceState {
                    issued: now,
                    conn,
                    last_nc: 0,
                },
            );
        }
        let stale = if stale { ", stale=true" } else { "" };
        Ok([Algorithm::Sha256, Algorithm::Md5]
            .into_iter()
            .map(|a| {
                format!(
                    "Digest realm=\"{}\", nonce=\"{nonce}\", algorithm={}, qop=\"auth\"{stale}",
                    self.credentials.realm,
                    a.token()
                )
            })
            .collect())
    }

    /// Verify the `Authorization` header of a request received on connection `conn`.
    #[must_use]
    pub fn authorize(
        &self, conn: ConnectionId, method: &str, uri: &str, authorization: Option<&str>,
        now: Instant,
    ) -> AuthOutcome {
        let Some(header) = authorization else {
            return challenge(false, false);
        };
        let Ok(p) = DigestParams::parse(header) else {
            return challenge(false, true);
        };
        let algorithm = match p.algorithm.as_deref() {
            None => Some(Algorithm::Md5),
            Some(token) => Algorithm::from_token(token),
        };
        let Some(algorithm) = algorithm else {
            return challenge(false, true);
        };
        let legacy = p.qop.is_empty();
        // `qop`-less is RFC 2069, which only ever existed for MD5.
        let nc = if legacy {
            (algorithm == Algorithm::Md5).then_some(0)
        } else {
            parse_nc(&p.nc).filter(|_| {
                p.qop.eq_ignore_ascii_case("auth")
                    && !p.cnonce.is_empty()
                    && p.cnonce.len() <= MAX_CNONCE_LEN
            })
        };
        let (Some(nc), true) = (nc, p.response.len() <= 64) else {
            return challenge(false, true);
        };

        // Evaluate every comparison before branching so timing does not reveal which field
        // was wrong.
        let user_ok = ct_eq(p.username.as_bytes(), self.credentials.username.as_bytes());
        let realm_ok = ct_eq(p.realm.as_bytes(), self.credentials.realm.as_bytes());
        let uri_ok = ct_eq(p.uri.as_bytes(), uri.as_bytes());
        let ha2 = algorithm.hash_hex(&[method.as_bytes(), b":", uri.as_bytes()]);
        let ha1 = self.credentials.ha1(algorithm).as_bytes();
        let expected = if legacy {
            algorithm.hash_hex(&[ha1, b":", p.nonce.as_bytes(), b":", ha2.as_bytes()])
        } else {
            algorithm.hash_hex(&[
                ha1,
                b":",
                p.nonce.as_bytes(),
                b":",
                p.nc.as_bytes(),
                b":",
                p.cnonce.as_bytes(),
                b":auth:",
                ha2.as_bytes(),
            ])
        };
        let digest_ok = ct_eq(
            expected.as_bytes(),
            p.response.to_ascii_lowercase().as_bytes(),
        );
        if !(user_ok & realm_ok & uri_ok & digest_ok) {
            return challenge(false, true);
        }

        let mut nonces = self.nonces.lock().unwrap_or_else(PoisonError::into_inner);
        match nonces.get_mut(&p.nonce) {
            Some(state) if now.saturating_duration_since(state.issued) < self.nonce_lifetime => {
                if legacy {
                    // No `nc`: the nonce is only good on the connection it was issued on.
                    if state.conn == conn {
                        AuthOutcome::Authorized
                    } else {
                        challenge(true, false)
                    }
                } else if nc > state.last_nc {
                    state.last_nc = nc;
                    AuthOutcome::Authorized
                } else {
                    // Replay of a request already served on this nonce.
                    challenge(false, true)
                }
            }
            Some(_) => {
                nonces.remove(&p.nonce);
                challenge(true, false)
            }
            None => challenge(true, false),
        }
    }
}

fn parse_nc(nc: &str) -> Option<u32> {
    if nc.len() != 8 || !nc.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(nc, 16).ok()
}

#[derive(Default)]
struct DigestParams {
    username: String,
    realm: String,
    nonce: String,
    uri: String,
    response: String,
    algorithm: Option<String>,
    qop: String,
    nc: String,
    cnonce: String,
}

impl fmt::Debug for DigestParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DigestParams")
            .field("contents", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
enum DigestParseError {
    #[error("Digest header too long")]
    TooLong,
    #[error("invalid Digest scheme")]
    Scheme,
    #[error("malformed Digest parameter")]
    Parameter,
    #[error("duplicate Digest parameter")]
    Duplicate,
}

impl DigestParams {
    /// Parse `Digest k=v, k="quoted", ...`.
    fn parse(header: &str) -> Result<Self, DigestParseError> {
        if header.len() > MAX_HEADER_LEN {
            return Err(DigestParseError::TooLong);
        }
        let (scheme, mut rest) = header
            .trim()
            .split_once(|c: char| c.is_ascii_whitespace())
            .ok_or(DigestParseError::Scheme)?;
        if !scheme.eq_ignore_ascii_case("digest") {
            return Err(DigestParseError::Scheme);
        }
        let mut params = Self::default();
        let mut seen: Vec<String> = Vec::new();
        loop {
            rest = rest.trim_start_matches([' ', '\t', ',']);
            if rest.is_empty() {
                break;
            }
            let (key, after) = rest.split_once('=').ok_or(DigestParseError::Parameter)?;
            let key = key.trim().to_ascii_lowercase();
            let after = after.trim_start();
            let (value, remainder) = if let Some(quoted) = after.strip_prefix('"') {
                let mut value = String::new();
                let mut chars = quoted.char_indices();
                loop {
                    match chars.next().ok_or(DigestParseError::Parameter)? {
                        (_, '"') => break,
                        (_, '\\') => value.push(chars.next().ok_or(DigestParseError::Parameter)?.1),
                        (_, c) => value.push(c),
                    }
                }
                (value, chars.as_str())
            } else {
                let (value, remainder) = after.split_once(',').unwrap_or((after, ""));
                (value.trim().to_owned(), remainder)
            };
            if seen.contains(&key) {
                return Err(DigestParseError::Duplicate);
            }
            match key.as_str() {
                "username" => params.username = value,
                "realm" => params.realm = value,
                "nonce" => params.nonce = value,
                "uri" => params.uri = value,
                "response" => params.response = value,
                "algorithm" => params.algorithm = Some(value),
                "qop" => params.qop = value,
                "nc" => params.nc = value,
                "cnonce" => params.cnonce = value,
                _ => {}
            }
            seen.push(key);
            rest = remainder;
        }
        Ok(params)
    }
}

/// Inputs for a client-side `Authorization` header. Used by tests and by tools that talk to
/// this server.
#[derive(Clone, Copy)]
pub struct ClientDigest<'a> {
    /// Hash algorithm to answer with.
    pub algorithm: Algorithm,
    /// Account name.
    pub username: &'a str,
    /// Account password.
    pub password: &'a str,
    /// Realm from the challenge.
    pub realm: &'a str,
    /// Nonce from the challenge.
    pub nonce: &'a str,
    /// Request method.
    pub method: &'a str,
    /// Request URI as sent.
    pub uri: &'a str,
    /// Nonce count; must increase on every request that reuses `nonce`.
    pub nc: u32,
    /// Client nonce.
    pub cnonce: &'a str,
}

impl fmt::Debug for ClientDigest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientDigest")
            .field("username", &self.username)
            .field("realm", &self.realm)
            .field("password", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl ClientDigest<'_> {
    /// An RFC 2069 `Authorization` value: no `qop`, `nc`, `cnonce` or `algorithm`, response
    /// `MD5(HA1:nonce:HA2)`. What live555-based clients send. Ignores `algorithm`, `nc` and
    /// `cnonce`.
    #[must_use]
    pub fn legacy_header_value(&self) -> String {
        let Self {
            username,
            password,
            realm,
            nonce,
            method,
            uri,
            ..
        } = *self;
        let md5 = Algorithm::Md5;
        let ha1 = md5.hash_hex(&[format!("{username}:{realm}:{password}").as_bytes()]);
        let ha2 = md5.hash_hex(&[format!("{method}:{uri}").as_bytes()]);
        let response = md5.hash_hex(&[format!("{ha1}:{nonce}:{ha2}").as_bytes()]);
        format!(
            "Digest username=\"{username}\", realm=\"{realm}\", nonce=\"{nonce}\", \
             uri=\"{uri}\", response=\"{response}\""
        )
    }

    /// The `Authorization` header value.
    #[must_use]
    pub fn header_value(&self) -> String {
        let Self {
            algorithm,
            username,
            password,
            realm,
            nonce,
            method,
            uri,
            nc,
            cnonce,
        } = *self;
        let ha1 = algorithm.hash_hex(&[format!("{username}:{realm}:{password}").as_bytes()]);
        let ha2 = algorithm.hash_hex(&[format!("{method}:{uri}").as_bytes()]);
        let nc = format!("{nc:08x}");
        let response =
            algorithm.hash_hex(&[format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}").as_bytes()]);
        format!(
            "Digest username=\"{username}\", realm=\"{realm}\", nonce=\"{nonce}\", \
             uri=\"{uri}\", algorithm={}, qop=auth, nc={nc}, cnonce=\"{cnonce}\", \
             response=\"{response}\"",
            algorithm.token()
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn malformed_digest_returns_typed_error() {
        use super::{DigestParams, DigestParseError};
        assert!(matches!(
            DigestParams::parse("not digest"),
            Err(DigestParseError::Scheme)
        ));
        assert!(matches!(
            DigestParams::parse(&"x".repeat(super::MAX_HEADER_LEN + 1)),
            Err(DigestParseError::TooLong)
        ));
        assert!(matches!(
            DigestParams::parse("Digest bad"),
            Err(DigestParseError::Parameter)
        ));
        assert!(matches!(
            DigestParams::parse("Digest x=1,x=2"),
            Err(DigestParseError::Duplicate)
        ));
        assert!(matches!(
            DigestParams::parse("Digest x=\"unterminated"),
            Err(DigestParseError::Parameter)
        ));
        assert!(matches!(
            DigestParams::parse("Digest x=\"trailing\\"),
            Err(DigestParseError::Parameter)
        ));
    }

    #[test]
    fn digest_parser_decodes_quoted_escapes() {
        let params = super::DigestParams::parse(r#"Digest username="a\"b\\c""#).unwrap();
        assert_eq!(params.username, "a\"b\\c");
    }

    #[test]
    fn parsed_digest_debug_redacts_response() {
        let params = super::DigestParams::parse("Digest response=synthetic-secret").unwrap();
        assert!(!format!("{params:?}").contains("synthetic-secret"));
    }
    use proptest::prelude::*;

    use super::*;

    const REALM: &str = "tethercam";
    const CONN: ConnectionId = 1;
    const URI: &str = "rtsp://192.0.2.10/stream";

    fn auth() -> Authenticator {
        let creds = Credentials::new("camera", "s3cret-pass", REALM).unwrap();
        Authenticator::new(creds, Duration::from_secs(60))
    }

    fn nonce_of(challenge: &str) -> String {
        let start = challenge.find("nonce=\"").unwrap() + 7;
        let rest = &challenge[start..];
        rest[..rest.find('"').unwrap()].to_owned()
    }

    fn header(alg: Algorithm, password: &str, nonce: &str, method: &str, nc: u32) -> String {
        ClientDigest {
            algorithm: alg,
            username: "camera",
            password,
            realm: REALM,
            nonce,
            method,
            uri: URI,
            nc,
            cnonce: "0a4f113b",
        }
        .header_value()
    }

    fn nonce(a: &Authenticator, now: Instant) -> String {
        nonce_of(&a.challenge(CONN, false, now).unwrap()[0])
    }

    #[test]
    fn known_hash_vectors() {
        assert_eq!(
            Algorithm::Md5.hash_hex(&[b"abc"]),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        assert_eq!(
            Algorithm::Sha256.hash_hex(&[b"a", b"bc"]),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn rfc7616_md5_example_response() {
        // RFC 7616 section 3.9.1 (MD5), recomputed with the RFC's inputs.
        let ha1 = Algorithm::Md5.hash_hex(&[b"Mufasa:http-auth@example.org:Circle of Life"]);
        let ha2 = Algorithm::Md5.hash_hex(&[b"GET:/dir/index.html"]);
        let response = Algorithm::Md5.hash_hex(&[
            format!(
                "{ha1}:7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v:00000001:f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ:auth:{ha2}"
            )
            .as_bytes(),
        ]);
        assert_eq!(response, "8ca523f5e9506fed4657c9700eebdbec");
    }

    #[test]
    fn challenge_offers_both_algorithms_with_one_nonce() {
        let a = auth();
        let c = a.challenge(CONN, false, Instant::now()).unwrap();
        assert_eq!(c.len(), 2);
        assert!(c[0].contains("algorithm=SHA-256") && c[1].contains("algorithm=MD5"));
        assert!(
            c.iter()
                .all(|h| h.contains("qop=\"auth\"") && !h.contains("stale"))
        );
        assert_eq!(nonce_of(&c[0]), nonce_of(&c[1]));
        assert!(a.challenge(CONN, true, Instant::now()).unwrap()[0].ends_with("stale=true"));
    }

    #[test]
    fn nonces_are_unique() {
        let a = auth();
        let now = Instant::now();
        assert_ne!(nonce(&a, now), nonce(&a, now));
    }

    #[test]
    fn accepts_valid_digest_for_both_algorithms_and_every_method() {
        let a = auth();
        let now = Instant::now();
        let n = nonce(&a, now);
        let mut nc = 0;
        for alg in [Algorithm::Sha256, Algorithm::Md5] {
            for method in [
                "OPTIONS",
                "DESCRIBE",
                "SETUP",
                "PLAY",
                "TEARDOWN",
                "GET_PARAMETER",
            ] {
                nc += 1;
                let h = header(alg, "s3cret-pass", &n, method, nc);
                assert_eq!(
                    a.authorize(CONN, method, URI, Some(&h), now),
                    AuthOutcome::Authorized
                );
            }
        }
    }

    #[test]
    fn missing_header_is_a_plain_challenge() {
        assert_eq!(
            auth().authorize(CONN, "OPTIONS", URI, None, Instant::now()),
            challenge(false, false)
        );
    }

    #[test]
    fn rejects_wrong_password_user_method_and_uri() {
        let a = auth();
        let now = Instant::now();
        let n = nonce(&a, now);
        let bad = challenge(false, true);
        let wrong_pw = header(Algorithm::Sha256, "nope", &n, "PLAY", 1);
        assert_eq!(a.authorize(CONN, "PLAY", URI, Some(&wrong_pw), now), bad);
        let ok = header(Algorithm::Sha256, "s3cret-pass", &n, "PLAY", 1);
        // Digest bound to another method or URI.
        assert_eq!(a.authorize(CONN, "TEARDOWN", URI, Some(&ok), now), bad);
        assert_eq!(
            a.authorize(CONN, "PLAY", "rtsp://192.0.2.10/other", Some(&ok), now),
            bad
        );
        let other_user = ok.replace("username=\"camera\"", "username=\"root\"");
        assert_eq!(a.authorize(CONN, "PLAY", URI, Some(&other_user), now), bad);
        // The right header still works afterwards: failures do not burn the nonce.
        assert_eq!(
            a.authorize(CONN, "PLAY", URI, Some(&ok), now),
            AuthOutcome::Authorized
        );
    }

    #[test]
    fn rejects_replayed_and_reordered_nc() {
        let a = auth();
        let now = Instant::now();
        let n = nonce(&a, now);
        let two = header(Algorithm::Md5, "s3cret-pass", &n, "DESCRIBE", 2);
        assert_eq!(
            a.authorize(CONN, "DESCRIBE", URI, Some(&two), now),
            AuthOutcome::Authorized
        );
        assert_eq!(
            a.authorize(CONN, "DESCRIBE", URI, Some(&two), now),
            challenge(false, true)
        );
        let one = header(Algorithm::Md5, "s3cret-pass", &n, "DESCRIBE", 1);
        assert_eq!(
            a.authorize(CONN, "DESCRIBE", URI, Some(&one), now),
            challenge(false, true)
        );
    }

    #[test]
    fn expired_and_unknown_nonces_are_stale_only_with_a_correct_digest() {
        let a = auth();
        let now = Instant::now();
        let n = nonce(&a, now);
        let later = now + Duration::from_secs(61);
        let good = header(Algorithm::Sha256, "s3cret-pass", &n, "PLAY", 1);
        assert_eq!(
            a.authorize(CONN, "PLAY", URI, Some(&good), later),
            challenge(true, false)
        );
        let unknown = header(Algorithm::Sha256, "s3cret-pass", "deadbeef", "PLAY", 1);
        assert_eq!(
            a.authorize(CONN, "PLAY", URI, Some(&unknown), now),
            challenge(true, false)
        );
        let wrong = header(Algorithm::Sha256, "nope", &n, "PLAY", 1);
        assert_eq!(
            a.authorize(CONN, "PLAY", URI, Some(&wrong), later),
            challenge(false, true)
        );
    }

    #[test]
    fn nonce_table_is_bounded() {
        let creds = Credentials::new("camera", "s3cret-pass", REALM).unwrap();
        let a = Authenticator::with_nonce_capacity(creds, Duration::from_secs(60), 2);
        let now = Instant::now();
        let first = nonce(&a, now);
        let _second = nonce(&a, now + Duration::from_millis(1));
        let _third = nonce(&a, now + Duration::from_millis(2));
        assert_eq!(a.nonces.lock().unwrap().len(), 2);
        let h = header(Algorithm::Md5, "s3cret-pass", &first, "PLAY", 1);
        // Evicted nonce looks stale (correct digest, unknown nonce).
        assert_eq!(
            a.authorize(CONN, "PLAY", URI, Some(&h), now),
            challenge(true, false)
        );
    }

    #[test]
    fn rejects_unsupported_shapes() {
        let a = auth();
        let now = Instant::now();
        let n = nonce(&a, now);
        let good = header(Algorithm::Sha256, "s3cret-pass", &n, "PLAY", 1);
        let bad = challenge(false, true);
        for mutated in [
            good.replace("Digest", "Basic"),
            good.replace("qop=auth", "qop=auth-int"),
            good.replace("algorithm=SHA-256", "algorithm=SHA-512"),
            good.replace("nc=00000001", "nc=1"),
            good.replace("nc=00000001", "nc=zzzzzzzz"),
            good.replace(", cnonce=\"0a4f113b\"", ", cnonce=\"\""),
            format!("{good}, nc=00000002"),
            "Digest".to_owned(),
            "Digest username=\"unterminated".to_owned(),
            "x".repeat(2000),
        ] {
            assert_eq!(
                a.authorize(CONN, "PLAY", URI, Some(&mutated), now),
                bad,
                "{mutated}"
            );
        }
    }

    fn legacy(password: &str, nonce: &str, method: &str) -> String {
        ClientDigest {
            algorithm: Algorithm::Md5,
            username: "camera",
            password,
            realm: REALM,
            nonce,
            method,
            uri: URI,
            nc: 0,
            cnonce: "",
        }
        .legacy_header_value()
    }

    #[test]
    fn legacy_md5_without_qop_is_accepted_on_the_issuing_connection() {
        let a = auth();
        let now = Instant::now();
        let n = nonce(&a, now);
        // Reusable within the connection and lifetime, as RFC 2069 clients do.
        for method in ["OPTIONS", "DESCRIBE", "SETUP", "PLAY"] {
            let h = legacy("s3cret-pass", &n, method);
            assert_eq!(
                a.authorize(CONN, method, URI, Some(&h), now),
                AuthOutcome::Authorized
            );
            assert_eq!(
                a.authorize(CONN, method, URI, Some(&h), now),
                AuthOutcome::Authorized
            );
        }
        // The `qop=auth` path still works on the same nonce afterwards.
        let h = header(Algorithm::Md5, "s3cret-pass", &n, "PLAY", 1);
        assert_eq!(
            a.authorize(CONN, "PLAY", URI, Some(&h), now),
            AuthOutcome::Authorized
        );
    }

    #[test]
    fn legacy_md5_rejects_wrong_password_method_and_sha256() {
        let a = auth();
        let now = Instant::now();
        let n = nonce(&a, now);
        let bad = challenge(false, true);
        let wrong = legacy("nope", &n, "PLAY");
        assert_eq!(a.authorize(CONN, "PLAY", URI, Some(&wrong), now), bad);
        let other_method = legacy("s3cret-pass", &n, "TEARDOWN");
        assert_eq!(
            a.authorize(CONN, "PLAY", URI, Some(&other_method), now),
            bad
        );
        // SHA-256 is qop-only: a qop-less header naming it is rejected.
        let sha =
            legacy("s3cret-pass", &n, "PLAY").replace("response=", "algorithm=SHA-256, response=");
        assert_eq!(a.authorize(CONN, "PLAY", URI, Some(&sha), now), bad);
        // `qop=auth` headers missing `nc` or `cnonce` stay rejected.
        let no_nc =
            header(Algorithm::Md5, "s3cret-pass", &n, "PLAY", 1).replace("nc=00000001, ", "");
        assert_eq!(a.authorize(CONN, "PLAY", URI, Some(&no_nc), now), bad);
    }

    #[test]
    fn legacy_nonce_is_bound_to_its_connection_and_lifetime() {
        let a = auth();
        let now = Instant::now();
        let n = nonce(&a, now);
        let h = legacy("s3cret-pass", &n, "PLAY");
        // Another connection: correct digest, wrong connection -> stale, then a fresh nonce works.
        assert_eq!(
            a.authorize(2, "PLAY", URI, Some(&h), now),
            challenge(true, false)
        );
        let fresh = nonce_of(&a.challenge(2, true, now).unwrap()[1]);
        let h2 = legacy("s3cret-pass", &fresh, "PLAY");
        assert_eq!(
            a.authorize(2, "PLAY", URI, Some(&h2), now),
            AuthOutcome::Authorized
        );
        // The first connection's nonce is not usable on connection 2 and vice versa.
        assert_eq!(
            a.authorize(CONN, "PLAY", URI, Some(&h2), now),
            challenge(true, false)
        );
        // Expired.
        let later = now + Duration::from_secs(61);
        assert_eq!(
            a.authorize(CONN, "PLAY", URI, Some(&h), later),
            challenge(true, false)
        );
    }

    #[test]
    fn missing_algorithm_means_md5() {
        let a = auth();
        let now = Instant::now();
        let n = nonce(&a, now);
        let h = header(Algorithm::Md5, "s3cret-pass", &n, "PLAY", 1).replace(" algorithm=MD5,", "");
        assert_eq!(
            a.authorize(CONN, "PLAY", URI, Some(&h), now),
            AuthOutcome::Authorized
        );
    }

    #[test]
    fn authenticator_debug_redacts_challenge_nonce() {
        let auth = auth();
        let issued = nonce(&auth, Instant::now());
        let debug = format!("{auth:?}");
        assert!(!debug.contains(&issued));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn credentials_validate_and_redact() {
        assert!(Credentials::new("", "p", "r").is_err());
        assert!(Credentials::new("a:b", "p", "r").is_err());
        assert!(Credentials::new("a", "", "r").is_err());
        assert!(Credentials::new("a", "p", "r\"x").is_err());
        let dbg = format!(
            "{:?}",
            Credentials::new("camera", "s3cret-pass", REALM).unwrap()
        );
        assert!(!dbg.contains("s3cret-pass") && dbg.contains("redacted"));
        let adbg = format!("{:?}", auth());
        assert!(!adbg.contains("s3cret-pass"));
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    proptest! {
        #[test]
        fn authorization_parser_never_panics(header in ".{0,1200}") {
            let _ = DigestParams::parse(&header);
            let _ = auth().authorize(CONN, "PLAY", URI, Some(&header), Instant::now());
        }

        #[test]
        fn authorization_parser_never_panics_on_digest_shaped_input(
            body in "[a-z=\",\\\\ 0-9-]{0,300}",
        ) {
            let _ = DigestParams::parse(&format!("Digest {body}"));
        }
    }
}
