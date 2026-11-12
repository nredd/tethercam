//! WS-Security `UsernameToken` with `PasswordDigest`.
//!
//! `PasswordDigest = Base64(SHA1(nonce + created + password))` where `nonce` is the decoded
//! bytes and `created` the exact string the client sent. The verifier enforces:
//!
//! - a bounded clock-skew window on `Created`
//! - a nonce replay cache, filled only by tokens that otherwise verified, bounded in size and
//!   failing closed when full
//! - constant-time comparison of username and digest
//! - `PasswordText` is refused
//!
//! References:
//! - [WS-Security UsernameToken Profile 1.1](https://docs.oasis-open.org/wss/v1.1/wss-v1.1-spec-os-UsernameTokenProfile.pdf)
//! - [WS-Security 1.1](https://docs.oasis-open.org/wss/v1.1/wss-v1.1-spec-os-SOAPMessageSecurity.pdf)

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha1::{Digest, Sha1};

use super::{AuthError, Credentials, ct_eq};
use crate::onvif::soap::Element;
use crate::onvif::time::{Clock, parse_datetime};

/// WS-Security extension namespace.
pub const NS_WSSE: &str =
    "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd";
/// WS-Security utility namespace (`Created`).
pub const NS_WSU: &str =
    "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd";
/// Password type URI suffix for digests.
const DIGEST_SUFFIX: &str = "#PasswordDigest";
/// Decoded nonce length bounds.
const NONCE_LEN: std::ops::RangeInclusive<usize> = 8..=64;

/// How the password was presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordKind {
    /// `#PasswordDigest`.
    Digest,
    /// `#PasswordText`, or no `Type` attribute (the profile's default).
    Text,
}

/// A parsed `UsernameToken`. Fields are as sent, unverified.
#[derive(Clone, PartialEq, Eq)]
pub struct UsernameToken {
    username: String,
    password: String,
    kind: PasswordKind,
    nonce: Option<String>,
    created: Option<String>,
}

impl std::fmt::Debug for UsernameToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsernameToken")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl UsernameToken {
    /// Extract the token from a `wsse:Security` header block.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Malformed`] when the structure is not a `UsernameToken` with
    /// `Username` and `Password`.
    pub fn from_security(security: &Element) -> Result<Self, AuthError> {
        if !security.is(NS_WSSE, "Security") {
            return Err(AuthError::Malformed("not a Security header"));
        }
        let token = security
            .child_ns(NS_WSSE, "UsernameToken")
            .ok_or(AuthError::Malformed("no UsernameToken"))?;
        let username = token
            .child_ns(NS_WSSE, "Username")
            .ok_or(AuthError::Malformed("no Username"))?;
        let password = token
            .child_ns(NS_WSSE, "Password")
            .ok_or(AuthError::Malformed("no Password"))?;
        let kind = match password.attribute("Type") {
            Some(t) if t.ends_with(DIGEST_SUFFIX) => PasswordKind::Digest,
            _ => PasswordKind::Text,
        };
        Ok(Self {
            username: username.text.trim().to_owned(),
            password: password.text.trim().to_owned(),
            kind,
            nonce: token
                .child_ns(NS_WSSE, "Nonce")
                .map(|n| n.text.trim().to_owned()),
            created: token
                .child_ns(NS_WSU, "Created")
                .map(|c| c.text.trim().to_owned()),
        })
    }

    /// Build a digest token the way a client does. Used by tests, benchmarks and examples.
    #[must_use]
    pub fn digest_for(username: &str, password: &str, nonce: &[u8], created: &str) -> Self {
        Self {
            username: username.to_owned(),
            password: STANDARD.encode(compute_digest(nonce, created, password.as_bytes())),
            kind: PasswordKind::Digest,
            nonce: Some(STANDARD.encode(nonce)),
            created: Some(created.to_owned()),
        }
    }

    /// Serialize as a `wsse:Security` header block. Used by tests and benchmarks.
    #[must_use]
    pub fn to_header_xml(&self) -> String {
        use crate::onvif::soap::escape;
        format!(
            concat!(
                r#"<wsse:Security xmlns:wsse="{wsse}" xmlns:wsu="{wsu}"><wsse:UsernameToken>"#,
                "<wsse:Username>{user}</wsse:Username>",
                r#"<wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#{ty}">{pass}</wsse:Password>"#,
                "<wsse:Nonce>{nonce}</wsse:Nonce><wsu:Created>{created}</wsu:Created>",
                "</wsse:UsernameToken></wsse:Security>"
            ),
            wsse = NS_WSSE,
            wsu = NS_WSU,
            user = escape(&self.username),
            ty = match self.kind {
                PasswordKind::Digest => "PasswordDigest",
                PasswordKind::Text => "PasswordText",
            },
            pass = escape(&self.password),
            nonce = escape(self.nonce.as_deref().unwrap_or("")),
            created = escape(self.created.as_deref().unwrap_or("")),
        )
    }
}

fn compute_digest(nonce: &[u8], created: &str, password: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(nonce);
    h.update(created.as_bytes());
    h.update(password);
    h.finalize().into()
}

/// Verifier configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WsseConfig {
    /// Allowed distance of `Created` from now, either direction, in seconds.
    pub skew_seconds: i64,
    /// Maximum remembered nonces.
    pub max_nonces: usize,
}

impl Default for WsseConfig {
    fn default() -> Self {
        Self {
            skew_seconds: 300,
            max_nonces: 4096,
        }
    }
}

/// Verifies `UsernameToken` digests against one credential pair.
pub struct WsseVerifier {
    credentials: Credentials,
    clock: Arc<dyn Clock>,
    config: WsseConfig,
    /// Nonce bytes to the Unix second after which a replay can no longer pass the skew check.
    seen: Mutex<HashMap<Vec<u8>, i64>>,
}

impl std::fmt::Debug for WsseVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsseVerifier")
            .field("credentials", &self.credentials)
            .field("clock", &self.clock)
            .field("config", &self.config)
            .field("seen", &"<redacted>")
            .finish()
    }
}

impl WsseVerifier {
    /// A verifier with an empty replay cache.
    #[must_use]
    pub fn new(credentials: Credentials, clock: Arc<dyn Clock>, config: WsseConfig) -> Self {
        Self {
            credentials,
            clock,
            config,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Verify a token.
    ///
    /// # Errors
    ///
    /// Returns the specific [`AuthError`]; callers must not distinguish them to the client.
    pub fn verify(&self, token: &UsernameToken) -> Result<(), AuthError> {
        if token.kind != PasswordKind::Digest {
            return Err(AuthError::UnsupportedPasswordType);
        }
        let nonce_b64 = token
            .nonce
            .as_deref()
            .ok_or(AuthError::Malformed("no Nonce"))?;
        let created = token
            .created
            .as_deref()
            .ok_or(AuthError::Malformed("no Created"))?;
        let nonce = STANDARD
            .decode(nonce_b64)
            .map_err(|_| AuthError::Malformed("nonce not base64"))?;
        if !NONCE_LEN.contains(&nonce.len()) {
            return Err(AuthError::Malformed("nonce length"));
        }
        let created_at =
            parse_datetime(created).map_err(|_| AuthError::Malformed("bad Created"))?;
        let now = self.clock.now_unix();
        // A negative configured window is treated as zero, never as its absolute value.
        let skew = self.config.skew_seconds.max(0);
        if created_at.abs_diff(now) > skew.unsigned_abs() {
            return Err(AuthError::Stale);
        }
        let supplied = STANDARD
            .decode(&token.password)
            .map_err(|_| AuthError::Malformed("digest not base64"))?;

        // Evaluate both comparisons before branching so neither short-circuits the other.
        let expected = compute_digest(&nonce, created, self.credentials.password());
        let digest_ok = ct_eq(&expected, &supplied);
        let user_ok = self.credentials.username_matches(&token.username);
        if !(digest_ok & user_ok) {
            return Err(AuthError::Invalid);
        }

        // Only authentic tokens enter the cache, so unauthenticated traffic cannot fill it.
        let expiry = created_at.saturating_add(skew);
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.get(&nonce).is_some_and(|&until| until >= now) {
            return Err(AuthError::Replay);
        }
        if seen.len() >= self.config.max_nonces {
            seen.retain(|_, &mut until| until >= now);
            if seen.len() >= self.config.max_nonces {
                return Err(AuthError::ReplayCacheFull);
            }
        }
        seen.insert(nonce, expiry);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onvif::soap::{Envelope, NS_SOAP12, SoapLimits};
    use crate::onvif::time::{ManualClock, format_datetime};
    use proptest::prelude::*;

    const NOW: i64 = 1_750_000_000;
    const NONCE: &[u8] = b"0123456789abcdef";

    fn verifier(config: WsseConfig) -> (WsseVerifier, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(NOW));
        let v = WsseVerifier::new(
            Credentials::new("operator", "synthetic-pass").unwrap(),
            clock.clone(),
            config,
        );
        (v, clock)
    }

    fn token(password: &str, nonce: &[u8], at: i64) -> UsernameToken {
        UsernameToken::digest_for("operator", password, nonce, &format_datetime(at))
    }

    #[test]
    fn known_digest_vector() {
        // Cross-checked against an independent SHA-1 + base64 implementation (Python `hashlib`).
        let nonce = STANDARD.decode("LKqI6G/AikKCQrN0zqZFlg==").unwrap();
        let d = compute_digest(&nonce, "2010-01-01T00:00:00Z", b"taadtaadpstcsm");
        assert_eq!(STANDARD.encode(d), "PhPoKZgD32FDb44WZbw4+McI+QA=");
    }

    #[test]
    fn accepts_valid_token() {
        let (v, _) = verifier(WsseConfig::default());
        assert_eq!(v.verify(&token("synthetic-pass", NONCE, NOW)), Ok(()));
    }

    #[test]
    fn used_verifier_debug_hides_nonce() {
        let (v, _) = verifier(WsseConfig::default());
        assert_eq!(v.verify(&token("synthetic-pass", NONCE, NOW)), Ok(()));
        let debug = format!("{v:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(&format!("{NONCE:?}")));
    }

    #[test]
    fn rejects_wrong_password_and_user() {
        let (v, _) = verifier(WsseConfig::default());
        assert_eq!(
            v.verify(&token("wrong", NONCE, NOW)),
            Err(AuthError::Invalid)
        );
        let t =
            UsernameToken::digest_for("intruder", "synthetic-pass", NONCE, &format_datetime(NOW));
        assert_eq!(v.verify(&t), Err(AuthError::Invalid));
    }

    #[test]
    fn rejects_replayed_nonce_but_not_after_failure() {
        let (v, _) = verifier(WsseConfig::default());
        // A failed attempt must not burn the nonce for the legitimate client.
        assert!(v.verify(&token("wrong", NONCE, NOW)).is_err());
        assert_eq!(v.verify(&token("synthetic-pass", NONCE, NOW)), Ok(()));
        assert_eq!(
            v.verify(&token("synthetic-pass", NONCE, NOW)),
            Err(AuthError::Replay)
        );
    }

    #[test]
    fn skew_window_is_bounded_both_ways() {
        let (v, _) = verifier(WsseConfig::default());
        assert_eq!(
            v.verify(&token("synthetic-pass", b"nonce-001", NOW - 300)),
            Ok(())
        );
        assert_eq!(
            v.verify(&token("synthetic-pass", b"nonce-002", NOW + 300)),
            Ok(())
        );
        assert_eq!(
            v.verify(&token("synthetic-pass", b"nonce-003", NOW - 301)),
            Err(AuthError::Stale)
        );
        assert_eq!(
            v.verify(&token("synthetic-pass", b"nonce-004", NOW + 301)),
            Err(AuthError::Stale)
        );
    }

    #[test]
    fn replay_cache_expires_and_fails_closed() {
        let (v, clock) = verifier(WsseConfig {
            skew_seconds: 10,
            max_nonces: 2,
        });
        assert_eq!(
            v.verify(&token("synthetic-pass", b"nonce-001", NOW)),
            Ok(())
        );
        assert_eq!(
            v.verify(&token("synthetic-pass", b"nonce-002", NOW)),
            Ok(())
        );
        assert_eq!(
            v.verify(&token("synthetic-pass", b"nonce-003", NOW)),
            Err(AuthError::ReplayCacheFull)
        );
        clock.advance(11);
        assert_eq!(
            v.verify(&token("synthetic-pass", b"nonce-003", NOW + 11)),
            Ok(())
        );
        // The first nonce expired, and its Created is now stale anyway.
        assert_eq!(
            v.verify(&token("synthetic-pass", b"nonce-001", NOW)),
            Err(AuthError::Stale)
        );
    }

    #[test]
    fn negative_skew_is_zero_not_absolute() {
        let (v, _) = verifier(WsseConfig {
            skew_seconds: -300,
            max_nonces: 8,
        });
        assert_eq!(v.verify(&token("synthetic-pass", NONCE, NOW)), Ok(()));
        assert_eq!(
            v.verify(&token("synthetic-pass", NONCE, NOW)),
            Err(AuthError::Replay)
        );
        assert_eq!(
            v.verify(&token("synthetic-pass", b"nonce-002", NOW - 1)),
            Err(AuthError::Stale)
        );
    }

    #[test]
    fn rejects_plaintext_and_missing_fields() {
        let (v, _) = verifier(WsseConfig::default());
        let mut t = token("synthetic-pass", NONCE, NOW);
        t.kind = PasswordKind::Text;
        assert_eq!(v.verify(&t), Err(AuthError::UnsupportedPasswordType));
        let mut t = token("synthetic-pass", NONCE, NOW);
        t.nonce = None;
        assert_eq!(v.verify(&t), Err(AuthError::Malformed("no Nonce")));
        let mut t = token("synthetic-pass", NONCE, NOW);
        t.created = Some("yesterday".into());
        assert_eq!(v.verify(&t), Err(AuthError::Malformed("bad Created")));
        let mut t = token("synthetic-pass", NONCE, NOW);
        t.nonce = Some(STANDARD.encode(b"x"));
        assert_eq!(v.verify(&t), Err(AuthError::Malformed("nonce length")));
    }

    #[test]
    fn malformed_token_fields_and_security_structure() {
        let element = |name: &str, children| Element {
            namespace: Some(NS_WSSE.into()),
            name: name.into(),
            text: String::new(),
            attributes: vec![],
            children,
        };
        for (node, error) in [
            (element("other", vec![]), "not a Security header"),
            (element("Security", vec![]), "no UsernameToken"),
            (
                element("Security", vec![element("UsernameToken", vec![])]),
                "no Username",
            ),
            (
                element(
                    "Security",
                    vec![element("UsernameToken", vec![element("Username", vec![])])],
                ),
                "no Password",
            ),
        ] {
            assert_eq!(
                UsernameToken::from_security(&node),
                Err(AuthError::Malformed(error))
            );
        }
        let text_security = element(
            "Security",
            vec![element(
                "UsernameToken",
                vec![element("Username", vec![]), element("Password", vec![])],
            )],
        );
        assert_eq!(
            UsernameToken::from_security(&text_security).unwrap().kind,
            PasswordKind::Text
        );
        let (v, _) = verifier(WsseConfig::default());
        let mut t = token("synthetic-pass", NONCE, NOW);
        t.created = None;
        assert_eq!(v.verify(&t), Err(AuthError::Malformed("no Created")));
        t.created = Some(format_datetime(NOW));
        t.nonce = Some("!".into());
        assert_eq!(v.verify(&t), Err(AuthError::Malformed("nonce not base64")));
        t.nonce = Some(STANDARD.encode(NONCE));
        t.password = "!".into();
        assert_eq!(v.verify(&t), Err(AuthError::Malformed("digest not base64")));
        t.kind = PasswordKind::Text;
        assert!(t.to_header_xml().contains("#PasswordText"));
    }

    #[test]
    fn token_roundtrips_through_the_envelope_parser() {
        let t = token("synthetic-pass", NONCE, NOW);
        let xml = format!(
            r#"<s:Envelope xmlns:s="{NS_SOAP12}"><s:Header>{}</s:Header><s:Body><a/></s:Body></s:Envelope>"#,
            t.to_header_xml()
        );
        let env = Envelope::parse(xml.as_bytes(), &SoapLimits::default()).unwrap();
        let sec = env.header_block(NS_WSSE, "Security").unwrap();
        assert_eq!(UsernameToken::from_security(sec).unwrap(), t);
    }

    #[test]
    fn debug_hides_password_digest() {
        let t = token("synthetic-pass", NONCE, NOW);
        assert!(!format!("{t:?}").contains(&t.password));
    }

    proptest! {
        #[test]
        fn verify_never_panics(
            user in ".*", pass in ".*", nonce in ".*", created in ".*",
            digest in proptest::bool::ANY,
        ) {
            let (v, _) = verifier(WsseConfig::default());
            let t = UsernameToken {
                username: user,
                password: pass,
                kind: if digest { PasswordKind::Digest } else { PasswordKind::Text },
                nonce: Some(nonce),
                created: Some(created),
            };
            let _ = v.verify(&t);
        }

        #[test]
        fn security_extraction_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            if let Ok(env) = Envelope::parse(&bytes, &SoapLimits::default()) {
                for e in env.header.iter().chain(std::iter::once(&env.operation)) {
                    let _ = UsernameToken::from_security(e);
                }
            }
        }
    }
}
