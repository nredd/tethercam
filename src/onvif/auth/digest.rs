//! HTTP Digest authentication (`qop=auth`, `algorithm=MD5`).
//!
//! Server nonces are stateless and self-authenticating: `{time}{counter}{mac}` where `mac` is
//! `SHA1(secret || time || counter)` under a per-process random secret. A nonce expires after
//! [`NONCE_LIFETIME_SECONDS`]. The nonce count (`nc`) must strictly increase per nonce, which
//! bounds replay of a captured `Authorization` header; the table is bounded and fails closed.
//!
//! `qop` is mandatory: the RFC 2069 legacy form with no `cnonce` is refused.
//!
//! References:
//! - [HTTP Digest, RFC 7616](https://www.rfc-editor.org/rfc/rfc7616)
//! - [HTTP Digest (MD5), RFC 2617](https://www.rfc-editor.org/rfc/rfc2617)

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use md5::Md5;
use sha1::{Digest, Sha1};

use super::{AuthError, Credentials, ct_eq};
use crate::onvif::time::Clock;

/// How long a server nonce stays valid.
pub const NONCE_LIFETIME_SECONDS: i64 = 300;
/// Maximum nonces tracked for `nc` ordering.
const MAX_TRACKED_NONCES: usize = 1024;
/// Maximum `Authorization` header length considered.
const MAX_HEADER_LEN: usize = 2048;

/// Parsed `Authorization: Digest` parameters.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct DigestParams {
    /// `username`.
    pub username: String,
    /// `realm`.
    pub realm: String,
    /// `nonce`.
    pub nonce: String,
    /// `uri`.
    pub uri: String,
    /// `response`.
    pub response: String,
    /// `qop`.
    pub qop: String,
    /// `nc`.
    pub nc: String,
    /// `cnonce`.
    pub cnonce: String,
    /// `algorithm`, if sent.
    pub algorithm: Option<String>,
}

impl std::fmt::Debug for DigestParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DigestParams")
            .field("contents", &"<redacted>")
            .finish()
    }
}

/// Parse an `Authorization` header value. Total.
///
/// # Errors
///
/// Returns [`AuthError::Malformed`] for anything that is not a well-formed `Digest` header
/// with every required parameter.
pub fn parse_authorization(value: &str) -> Result<DigestParams, AuthError> {
    const BAD: AuthError = AuthError::Malformed("digest header");
    if value.len() > MAX_HEADER_LEN {
        return Err(BAD);
    }
    let (scheme, rest) = value.trim().split_once(' ').ok_or(BAD)?;
    if !scheme.eq_ignore_ascii_case("digest") {
        return Err(BAD);
    }
    let mut params = DigestParams::default();
    let mut seen = 0_u8;
    let mut chars = rest.trim().chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace() || *c == ',').is_some() {}
        if chars.peek().is_none() {
            break;
        }
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| *c != '=' && *c != ',') {
            key.push(c);
        }
        chars.next_if_eq(&'=').ok_or(BAD)?;
        let mut val = String::new();
        if chars.next_if_eq(&'"').is_some() {
            loop {
                match chars.next().ok_or(BAD)? {
                    '"' => break,
                    '\\' => val.push(chars.next().ok_or(BAD)?),
                    c => val.push(c),
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| *c != ',') {
                val.push(c);
            }
            val.truncate(val.trim_end().len());
        }
        let (slot, idx) = match key.trim().to_ascii_lowercase().as_str() {
            "username" => (&mut params.username, 0),
            "realm" => (&mut params.realm, 1),
            "nonce" => (&mut params.nonce, 2),
            "uri" => (&mut params.uri, 3),
            "response" => (&mut params.response, 4),
            "qop" => (&mut params.qop, 5),
            "nc" => (&mut params.nc, 6),
            "cnonce" => (&mut params.cnonce, 7),
            "algorithm" => {
                params.algorithm = Some(val);
                continue;
            }
            _ => continue,
        };
        let mask = 1_u8 << idx;
        if seen & mask != 0 {
            return Err(BAD);
        }
        seen |= mask;
        *slot = val;
    }
    if seen == u8::MAX {
        Ok(params)
    } else {
        Err(BAD)
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing to a `String` cannot fail.
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn md5_hex(data: &str) -> String {
    hex(&Md5::digest(data.as_bytes()))
}

/// Issues Digest challenges and verifies responses.
pub struct DigestAuth {
    credentials: Credentials,
    realm: String,
    clock: Arc<dyn Clock>,
    secret: [u8; 32],
    counter: AtomicU64,
    /// Nonce to last accepted `nc`.
    nonce_counts: Mutex<HashMap<String, u32>>,
}

impl std::fmt::Debug for DigestAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DigestAuth")
            .field("realm", &self.realm)
            .field("secret", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl DigestAuth {
    /// Create with a fresh random secret.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Entropy`] when the system random source fails.
    pub fn new(
        credentials: Credentials, realm: impl Into<String>, clock: Arc<dyn Clock>,
    ) -> Result<Self, AuthError> {
        Self::with_entropy(credentials, realm, clock, getrandom::fill)
    }

    fn with_entropy(
        credentials: Credentials, realm: impl Into<String>, clock: Arc<dyn Clock>,
        fill: fn(&mut [u8]) -> Result<(), getrandom::Error>,
    ) -> Result<Self, AuthError> {
        let mut secret = [0_u8; 32];
        fill(&mut secret).map_err(|_| AuthError::Entropy)?;
        Ok(Self::with_secret(credentials, realm, clock, secret))
    }

    /// Create with a fixed secret, for deterministic tests.
    #[must_use]
    pub fn with_secret(
        credentials: Credentials, realm: impl Into<String>, clock: Arc<dyn Clock>, secret: [u8; 32],
    ) -> Self {
        Self {
            credentials,
            realm: realm.into(),
            clock,
            secret,
            counter: AtomicU64::new(0),
            nonce_counts: Mutex::new(HashMap::new()),
        }
    }

    fn mac(&self, time: u64, counter: u64) -> String {
        let mut h = Sha1::new();
        h.update(self.secret);
        h.update(time.to_be_bytes());
        h.update(counter.to_be_bytes());
        hex(&h.finalize())
    }

    fn mint_nonce(&self) -> String {
        let time = u64::try_from(self.clock.now_unix()).unwrap_or(0);
        let counter = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{time:016x}{counter:016x}{}", self.mac(time, counter))
    }

    /// Whether `nonce` is one this process minted (MAC valid) and has not expired. A nonce
    /// from before a restart, a forged one and an expired one are all simply not fresh.
    fn nonce_fresh(&self, nonce: &str) -> bool {
        let parse = |range: std::ops::Range<usize>| {
            nonce
                .get(range)
                .and_then(|h| u64::from_str_radix(h, 16).ok())
        };
        let (Some(time), Some(counter), Some(mac)) = (parse(0..16), parse(16..32), nonce.get(32..))
        else {
            return false;
        };
        if !ct_eq(mac.as_bytes(), self.mac(time, counter).as_bytes()) {
            return false;
        }
        let age = self
            .clock
            .now_unix()
            .saturating_sub(i64::try_from(time).unwrap_or(i64::MAX));
        (0..=NONCE_LIFETIME_SECONDS).contains(&age)
    }

    /// A `WWW-Authenticate` header value with a fresh nonce.
    #[must_use]
    pub fn challenge(&self, stale: bool) -> String {
        format!(
            r#"Digest realm="{}", qop="auth", algorithm=MD5, nonce="{}"{}"#,
            self.realm.replace(['"', '\\'], ""),
            self.mint_nonce(),
            if stale { ", stale=true" } else { "" }
        )
    }

    /// Verify an `Authorization` header for a request with this `method` and request-target.
    ///
    /// # Errors
    ///
    /// [`AuthError::Stale`] means the credentials were correct for a nonce that is expired,
    /// unknown or forged: answer with `challenge(true)`. Every other
    /// variant is a plain failure.
    pub fn verify(&self, method: &str, target: &str, authorization: &str) -> Result<(), AuthError> {
        let p = parse_authorization(authorization)?;
        if p.qop != "auth"
            || p.algorithm
                .as_deref()
                .is_some_and(|a| !a.eq_ignore_ascii_case("md5"))
        {
            return Err(AuthError::UnsupportedPasswordType);
        }
        let nc = u32::from_str_radix(&p.nc, 16)
            .ok()
            .filter(|_| p.nc.len() == 8)
            .ok_or(AuthError::Malformed("nc"))?;

        let password = String::from_utf8_lossy(self.credentials.password());
        let ha1 = md5_hex(&format!(
            "{}:{}:{password}",
            self.credentials.username(),
            self.realm
        ));
        let ha2 = md5_hex(&format!("{method}:{}", p.uri));
        let expected = md5_hex(&format!(
            "{ha1}:{}:{}:{}:auth:{ha2}",
            p.nonce, p.nc, p.cnonce
        ));
        let ok = ct_eq(
            expected.as_bytes(),
            p.response.to_ascii_lowercase().as_bytes(),
        ) & self.credentials.username_matches(&p.username)
            & ct_eq(p.realm.as_bytes(), self.realm.as_bytes())
            & ct_eq(p.uri.as_bytes(), target.as_bytes());
        if !ok {
            return Err(AuthError::Invalid);
        }
        // Credentials are right but the nonce is expired, forged or from before a restart:
        // RFC 7616 `stale=true`, so the client retries with a fresh nonce.
        if !self.nonce_fresh(&p.nonce) {
            return Err(AuthError::Stale);
        }

        let mut counts = self
            .nonce_counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if counts.len() >= MAX_TRACKED_NONCES && !counts.contains_key(&p.nonce) {
            let now = self.clock.now_unix();
            counts.retain(|n, _| {
                u64::from_str_radix(n.get(..16).unwrap_or(""), 16)
                    .ok()
                    .and_then(|t| i64::try_from(t).ok())
                    .is_some_and(|t| now.saturating_sub(t) <= NONCE_LIFETIME_SECONDS)
            });
            if counts.len() >= MAX_TRACKED_NONCES {
                return Err(AuthError::ReplayCacheFull);
            }
        }
        match counts.get(&p.nonce) {
            Some(&last) if nc <= last => return Err(AuthError::Replay),
            _ => {
                counts.insert(p.nonce, nc);
            }
        }
        Ok(())
    }

    /// Build the `Authorization` value a client would send. For tests, benches and examples.
    #[must_use]
    pub fn client_response(
        username: &str, password: &str, method: &str, target: &str, challenge: &str, nc: u32,
        cnonce: &str,
    ) -> Option<String> {
        let field = |name: &str| {
            let (_, after) = challenge.split_once(&format!("{name}=\""))?;
            after.split_once('"').map(|(v, _)| v.to_owned())
        };
        let (realm, nonce) = (field("realm")?, field("nonce")?);
        let ha1 = md5_hex(&format!("{username}:{realm}:{password}"));
        let ha2 = md5_hex(&format!("{method}:{target}"));
        let response = md5_hex(&format!("{ha1}:{nonce}:{nc:08x}:{cnonce}:auth:{ha2}"));
        Some(format!(
            r#"Digest username="{username}", realm="{realm}", nonce="{nonce}", uri="{target}", qop=auth, nc={nc:08x}, cnonce="{cnonce}", response="{response}", algorithm=MD5"#
        ))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parsed_digest_debug_redacts_response() {
        let value = "Digest username=alice, realm=test, nonce=abc, uri=/, response=synthetic-secret, qop=auth, nc=00000001, cnonce=abc";
        let params = super::parse_authorization(value).unwrap();
        assert!(!format!("{params:?}").contains("synthetic-secret"));
    }
    use super::*;
    use crate::onvif::time::ManualClock;
    use proptest::prelude::*;

    const URI: &str = "/onvif/snapshot/main";

    fn auth() -> (DigestAuth, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(1_750_000_000));
        let a = DigestAuth::with_secret(
            Credentials::new("operator", "synthetic-pass").unwrap(),
            "tethercam",
            clock.clone(),
            [7; 32],
        );
        (a, clock)
    }

    fn respond(pass: &str, nc: u32, challenge: &str) -> String {
        DigestAuth::client_response("operator", pass, "GET", URI, challenge, nc, "cn0nce").unwrap()
    }

    #[test]
    fn deterministic_error_and_cache_paths() {
        let creds = || Credentials::new("operator", "synthetic-pass").unwrap();
        let clock = Arc::new(ManualClock::new(1_750_000_000));
        assert_eq!(
            DigestAuth::with_entropy(creds(), "tethercam", clock.clone(), |_| Err(
                getrandom::Error::UNSUPPORTED
            ))
            .unwrap_err()
            .to_string(),
            AuthError::Entropy.to_string()
        );
        assert!(DigestAuth::with_entropy(creds(), "tethercam", clock, |_| Ok(())).is_ok());
        let (a, clock) = auth();
        let header = respond("synthetic-pass", 1, &a.challenge(false));
        assert_eq!(
            a.verify(
                "GET",
                URI,
                &header.replace("algorithm=MD5", "algorithm=SHA-256")
            ),
            Err(AuthError::UnsupportedPasswordType)
        );
        assert_eq!(
            a.verify("GET", URI, &header.replace("nc=00000001", "nc=1")),
            Err(AuthError::Malformed("nc"))
        );
        assert!(!a.nonce_fresh("bad"));
        for input in [
            format!("Digest {}", "a".repeat(MAX_HEADER_LEN)),
            "Digest key".into(),
            "Digest key=\"unterminated".into(),
            "Digest key=\"\\".into(),
        ] {
            assert!(parse_authorization(&input).is_err());
        }
        assert!(parse_authorization(&(header.clone() + ", ignored=value")).is_ok());
        assert!(DigestAuth::client_response("u", "p", "GET", "/", "missing", 1, "c").is_none());
        assert!(DigestAuth::client_response("u", "p", "GET", "/", "realm=\"x\"", 1, "c").is_none());
        let first = a.mint_nonce();
        {
            let mut cache = a.nonce_counts.lock().unwrap();
            for _ in 0..MAX_TRACKED_NONCES {
                cache.insert(a.mint_nonce(), 1);
            }
        }
        let challenge = format!("Digest realm=\"tethercam\", nonce=\"{first}\"");
        assert_eq!(
            a.verify("GET", URI, &respond("synthetic-pass", 1, &challenge)),
            Err(AuthError::ReplayCacheFull)
        );
        clock.advance(NONCE_LIFETIME_SECONDS + 1);
        let fresh = a.challenge(false);
        assert_eq!(
            a.verify("GET", URI, &respond("synthetic-pass", 1, &fresh)),
            Ok(())
        );
    }

    #[test]
    fn rfc2617_vector() {
        // RFC 2617 section 3.5 example, MD5, qop=auth.
        let ha1 = md5_hex("Mufasa:testrealm@host.com:Circle Of Life");
        let ha2 = md5_hex("GET:/dir/index.html");
        let r = md5_hex(&format!(
            "{ha1}:dcd98b7102dd2f0e8b11d0f600bfb0c093:00000001:0a4f113b:auth:{ha2}"
        ));
        assert_eq!(r, "6629fae49393a05397450978507c4ef1");
    }

    #[test]
    fn accepts_valid_then_rejects_replay_and_old_nc() {
        let (a, _) = auth();
        let ch = a.challenge(false);
        assert_eq!(
            a.verify("GET", URI, &respond("synthetic-pass", 1, &ch)),
            Ok(())
        );
        assert_eq!(
            a.verify("GET", URI, &respond("synthetic-pass", 2, &ch)),
            Ok(())
        );
        assert_eq!(
            a.verify("GET", URI, &respond("synthetic-pass", 2, &ch)),
            Err(AuthError::Replay)
        );
        assert_eq!(
            a.verify("GET", URI, &respond("synthetic-pass", 1, &ch)),
            Err(AuthError::Replay)
        );
    }

    #[test]
    fn rejects_wrong_password_method_and_uri() {
        let (a, _) = auth();
        let ch = a.challenge(false);
        assert_eq!(
            a.verify("GET", URI, &respond("wrong", 1, &ch)),
            Err(AuthError::Invalid)
        );
        assert_eq!(
            a.verify("POST", URI, &respond("synthetic-pass", 1, &ch)),
            Err(AuthError::Invalid)
        );
        assert_eq!(
            a.verify("GET", "/other", &respond("synthetic-pass", 1, &ch)),
            Err(AuthError::Invalid)
        );
    }

    #[test]
    fn forged_unknown_and_expired_nonces_are_stale_only_with_valid_credentials() {
        let (a, clock) = auth();
        let ch = a.challenge(false);
        let forged = ch.replace("nonce=\"0", "nonce=\"1");
        assert_eq!(
            a.verify("GET", URI, &respond("synthetic-pass", 1, &forged)),
            Err(AuthError::Stale)
        );
        // Wrong password is a plain failure even on a bad nonce: no stale oracle.
        assert_eq!(
            a.verify("GET", URI, &respond("wrong", 1, &forged)),
            Err(AuthError::Invalid)
        );
        // A nonce minted by a previous process (different secret) is unknown here.
        let old = DigestAuth::with_secret(
            Credentials::new("operator", "synthetic-pass").unwrap(),
            "tethercam",
            Arc::new(ManualClock::new(1_750_000_000)),
            [1; 32],
        )
        .challenge(false);
        assert_eq!(
            a.verify("GET", URI, &respond("synthetic-pass", 1, &old)),
            Err(AuthError::Stale)
        );
        clock.advance(NONCE_LIFETIME_SECONDS + 1);
        assert_eq!(
            a.verify("GET", URI, &respond("synthetic-pass", 1, &ch)),
            Err(AuthError::Stale)
        );
        assert!(a.challenge(true).contains("stale=true"));
    }

    #[test]
    fn debug_hides_secret_and_credentials() {
        let (a, _) = auth();
        let shown = format!("{a:?}");
        assert!(shown.contains("redacted"));
        assert!(!shown.contains("synthetic-pass"));
        assert!(!shown.contains("[7, 7"));
    }

    #[test]
    fn nonces_are_unique_and_secret_dependent() {
        let (a, _) = auth();
        assert_ne!(a.challenge(false), a.challenge(false));
        let clock = Arc::new(ManualClock::new(1_750_000_000));
        let b = DigestAuth::with_secret(
            Credentials::new("operator", "x").unwrap(),
            "tethercam",
            clock,
            [8; 32],
        );
        assert_ne!(a.mint_nonce().get(32..), b.mint_nonce().get(32..));
    }

    #[test]
    fn parser_handles_quoting_and_rejects_duplicates() {
        let h = r#"Digest username="o\"p", realm="r", nonce="n", uri="/", response="x", qop=auth, nc=00000001, cnonce="c""#;
        let p = parse_authorization(h).unwrap();
        assert_eq!(p.username, "o\"p");
        assert_eq!(p.qop, "auth");
        assert!(parse_authorization(&format!("{h}, realm=\"again\"")).is_err());
        assert!(parse_authorization("Basic abc").is_err());
        assert!(parse_authorization("Digest username=\"x\"").is_err());
    }

    proptest! {
        #[test]
        fn parse_never_panics(s in ".*") {
            let _ = parse_authorization(&s);
        }

        #[test]
        fn verify_never_panics(s in ".*", t in ".*") {
            let (a, _) = auth();
            let _ = a.verify("GET", &t, &s);
        }

        #[test]
        fn verify_never_panics_on_shaped_headers(
            nonce in "[0-9a-f]{0,80}", nc in "[0-9a-f]{0,10}", resp in "[0-9a-f]{0,40}",
        ) {
            let (a, _) = auth();
            let h = format!(
                r#"Digest username="operator", realm="tethercam", nonce="{nonce}", uri="/", response="{resp}", qop=auth, nc={nc}, cnonce="c""#
            );
            let _ = a.verify("GET", "/", &h);
        }
    }
}
