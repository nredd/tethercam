//! Authentication for the ONVIF endpoint.
//!
//! Two mechanisms, both against one [`Credentials`] pair:
//!
//! - [`wsse`]: WS-Security `UsernameToken` with `PasswordDigest`
//! - [`digest`]: HTTP Digest (`qop=auth`, MD5) as the alternative
//!
//! Credentials are compared in constant time and never appear in `Debug` output.
//!
//! References:
//! - [WS-Security UsernameToken Profile 1.1](https://docs.oasis-open.org/wss/v1.1/wss-v1.1-spec-os-UsernameTokenProfile.pdf)
//! - [HTTP Digest, RFC 7616](https://www.rfc-editor.org/rfc/rfc7616)

pub mod digest;
pub mod wsse;

use std::fmt;

use thiserror::Error;

/// Why authentication failed. On the wire every variant becomes the same `NotAuthorized`
/// fault; the distinction is for in-process callers and tests.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthError {
    /// No credentials were presented.
    #[error("no credentials presented")]
    Missing,
    /// `PasswordText` or an unknown password type.
    #[error("unsupported password type")]
    UnsupportedPasswordType,
    /// The security header is structurally unusable.
    #[error("malformed credentials: {0}")]
    Malformed(&'static str),
    /// `Created` is outside the clock-skew window.
    #[error("timestamp outside the allowed window")]
    Stale,
    /// The nonce was seen before inside the window.
    #[error("nonce replayed")]
    Replay,
    /// The replay cache is full of unexpired nonces. Fails closed.
    #[error("replay cache full")]
    ReplayCacheFull,
    /// Username or password digest does not match.
    #[error("invalid credentials")]
    Invalid,
    /// The system random source failed.
    #[error("random source unavailable")]
    Entropy,
}

/// A secret string whose `Debug` output is redacted.
#[derive(Clone)]
struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// The single username and password the endpoint accepts.
#[derive(Debug, Clone)]
pub struct Credentials {
    username: String,
    password: Secret,
}

impl Credentials {
    /// Build credentials. Both parts must be non-empty; there is no default.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Malformed`] when either part is empty.
    pub fn new(
        username: impl Into<String>, password: impl Into<String>,
    ) -> Result<Self, AuthError> {
        let (username, password) = (username.into(), password.into());
        if username.is_empty() || password.is_empty() {
            return Err(AuthError::Malformed("empty credential"));
        }
        Ok(Self {
            username,
            password: Secret(password),
        })
    }

    /// The username.
    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    pub(crate) fn password(&self) -> &[u8] {
        self.password.0.as_bytes()
    }

    /// Constant-time username comparison.
    pub(crate) fn username_matches(&self, candidate: &str) -> bool {
        ct_eq(self.username.as_bytes(), candidate.as_bytes())
    }
}

/// Constant-time equality. Runs over the longer input regardless of where they differ; only
/// the length difference itself is observable.
#[must_use]
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let len = a.len().max(b.len());
    let mut diff = u8::from(a.len() != b.len());
    for i in 0..len {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_semantics() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(ct_eq(b"", b""));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"abc", b""));
    }

    #[test]
    fn debug_redacts_password() {
        let c = Credentials::new("user", "hunter2-synthetic").unwrap();
        let shown = format!("{c:?}");
        assert!(!shown.contains("hunter2"));
        assert!(shown.contains("redacted"));
    }

    #[test]
    fn empty_credentials_rejected() {
        assert!(Credentials::new("", "x").is_err());
        assert!(Credentials::new("x", "").is_err());
    }
}
