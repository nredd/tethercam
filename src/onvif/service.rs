//! The transport-independent ONVIF request router.
//!
//! [`OnvifService::handle`] takes a parsed HTTP request and returns a response. It owns
//! routing, the pre-auth policy and authentication; [`http`](super::http) owns sockets.
//!
//! Pre-auth calls (no credentials needed): `GetSystemDateAndTime`, `GetCapabilities` and
//! `GetServices` on the Device service. Everything else needs WS-Security or HTTP Digest.
//! A request with no credentials at all gets `401` with a Digest challenge; a request whose
//! credentials are present but wrong gets the ONVIF `ter:NotAuthorized` fault.
//!
//! Failure shape depends on the mechanism, because Digest clients only retry on 401:
//! - A failed or stale HTTP Digest `Authorization` gets `401` plus a fresh challenge, with
//!   `stale=true` when the credentials were right for an expired or unknown nonce (RFC 7616)
//! - A failed WS-Security `UsernameToken` gets the `ter:NotAuthorized` fault with status 400
//! - Pre-auth calls are answered even when credentials are supplied but wrong or stale: a
//!   client with a skewed clock needs `GetSystemDateAndTime` to correct itself, and the reply
//!   holds nothing but time and URLs
//! - A request carrying both is judged by WS-Security alone; the `Authorization` header is
//!   ignored, so a good token succeeds and a bad token fails with the SOAP fault
//!
//! The three pre-auth calls are the ones the ONVIF Core Specification (access control, pre-auth
//! access class) requires a device to answer without credentials: clients need the time to
//! build a valid `Created`, and the service URLs to find the other endpoints. They expose
//! nothing but the time and URLs; `GetServices` ignores `IncludeCapability` without credentials.
//!
//! References:
//! - [ONVIF Core Specification](https://www.onvif.org/specs/core/ONVIF-Core-Specification.pdf)
//! - [WS-Security UsernameToken Profile 1.1](https://docs.oasis-open.org/wss/v1.1/wss-v1.1-spec-os-UsernameTokenProfile.pdf)

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use thiserror::Error;

use super::auth::digest::DigestAuth;
use super::auth::wsse::{NS_WSSE, UsernameToken, WsseConfig, WsseVerifier};
use super::auth::{AuthError, Credentials};
use super::device::{self, DEVICE_PATH};
use super::media::{self, MEDIA_PATH, SNAPSHOT_PREFIX, STATUS_PATH};
use super::model::{DeviceModel, StreamProfile};
use super::soap::{Element, Envelope, Fault, NS_DEVICE, SoapError, SoapLimits, envelope};
use super::time::Clock;

/// Digest realm.
pub const REALM: &str = "tethercam";
/// SOAP 1.2 content type.
const SOAP_CONTENT_TYPE: &str = "application/soap+xml; charset=utf-8";

/// A snapshot provider, implemented by the supervisor. A still briefly pauses live view.
pub trait SnapshotSource: Send + Sync + std::fmt::Debug {
    /// A JPEG for the profile with this token.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError`] when no frame is available.
    fn snapshot(&self, profile_token: &str) -> Result<Vec<u8>, SnapshotError>;
}

/// Camera state for the status endpoint, implemented by the supervisor. It must carry no device
/// identity (no serial, no address, no name): only whether a camera is serving and its power.
pub trait StatusSource: Send + Sync + std::fmt::Debug {
    /// A JSON document describing the camera state.
    fn status_json(&self) -> String;
}

/// No snapshot could be produced right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("snapshot unavailable")]
pub struct SnapshotError;

/// Why the service could not be constructed.
#[derive(Debug, Error)]
pub enum SetupError {
    /// The random source for Digest nonces failed.
    #[error("authentication setup failed: {0}")]
    Auth(#[from] AuthError),
}

/// A request as the router sees it.
#[derive(Clone, Copy)]
pub struct ServiceRequest<'a> {
    /// HTTP method.
    pub method: &'a str,
    /// Request-target as sent, including any query.
    pub target: &'a str,
    /// `Authorization` header value.
    pub authorization: Option<&'a str>,
    /// Request body.
    pub body: &'a [u8],
    /// Local address the connection arrived on; used to build advertised URLs.
    pub local: SocketAddr,
}

impl std::fmt::Debug for ServiceRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceRequest")
            .field("contents", &"<redacted>")
            .finish()
    }
}

/// A response to write back.
#[derive(Clone, PartialEq, Eq)]
pub struct ServiceResponse {
    /// HTTP status.
    pub status: u16,
    /// `Content-Type`.
    pub content_type: &'static str,
    /// Body.
    pub body: Vec<u8>,
    /// `WWW-Authenticate` value, for 401.
    pub www_authenticate: Option<String>,
}

impl std::fmt::Debug for ServiceResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceResponse")
            .field("status", &self.status)
            .field("content_type", &self.content_type)
            .field("body_len", &self.body.len())
            .field("www_authenticate", &"[REDACTED]")
            .finish()
    }
}

impl ServiceResponse {
    fn soap(status: u16, xml: String) -> Self {
        Self {
            status,
            content_type: SOAP_CONTENT_TYPE,
            body: xml.into_bytes(),
            www_authenticate: None,
        }
    }

    fn fault(fault: &Fault) -> Self {
        Self::soap(fault.http_status(), fault.to_envelope())
    }

    fn plain(status: u16) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: Vec::new(),
            www_authenticate: None,
        }
    }

    /// `503` for a service that is up but has no camera yet; clients retry. Answered before
    /// authentication on the SOAP services: a `401` would read as a credential failure to
    /// WS-Security clients, which retry transport errors but stop and ask the user on auth ones.
    fn not_ready() -> Self {
        Self {
            body: b"camera not ready".to_vec(),
            ..Self::plain(503)
        }
    }
}

/// Per-request view handed to the handlers.
pub(crate) struct Context<'a> {
    pub(crate) model: &'a DeviceModel,
    pub(crate) local: SocketAddr,
    pub(crate) now: i64,
    pub(crate) snapshot: bool,
    /// Whether the request carried valid credentials. Pre-auth calls reveal less without.
    pub(crate) authenticated: bool,
}

impl Context<'_> {
    fn base(&self, port: u16) -> SocketAddr {
        SocketAddr::new(self.local.ip(), port)
    }

    pub(crate) fn device_url(&self) -> String {
        format!("http://{}{DEVICE_PATH}", self.local)
    }

    pub(crate) fn media_url(&self) -> String {
        format!("http://{}{MEDIA_PATH}", self.local)
    }

    pub(crate) fn rtsp_url(&self, p: &StreamProfile) -> String {
        format!(
            "rtsp://{}{}",
            self.base(self.model.rtsp_port()),
            p.rtsp_path
        )
    }

    pub(crate) fn snapshot_url(&self, p: &StreamProfile) -> String {
        format!("http://{}{SNAPSHOT_PREFIX}{}", self.local, p.token)
    }

    pub(crate) fn media_capabilities_xml(&self) -> String {
        format!(
            "<trt:Capabilities SnapshotUri=\"{}\" Rotation=\"false\" VideoSourceMode=\"false\" OSD=\"false\">\
             <trt:ProfileCapabilities MaximumNumberOfProfiles=\"{}\"/>\
             <trt:StreamingCapabilities RTPMulticast=\"false\" RTP_TCP=\"true\" \
             RTP_RTSP_TCP=\"true\" NonAggregateControl=\"false\"/></trt:Capabilities>",
            self.snapshot,
            self.model.profiles().len()
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    Device,
    Media,
}

/// The ONVIF Device and Media services behind one request router.
#[derive(Debug)]
pub struct OnvifService {
    model: Arc<OnceLock<DeviceModel>>,
    clock: Arc<dyn Clock>,
    wsse: WsseVerifier,
    digest: DigestAuth,
    snapshot: Option<Arc<dyn SnapshotSource>>,
    status: Option<Arc<dyn StatusSource>>,
    limits: SoapLimits,
}

/// The one-shot right to publish a pending service's [`DeviceModel`]; see
/// [`OnvifService::pending`].
pub struct ModelPublisher(Arc<OnceLock<DeviceModel>>);

impl ModelPublisher {
    /// Start answering the Device and Media services and the snapshot endpoint with `model`.
    /// Consumes the publisher, so the model is set at most once.
    pub fn publish(self, model: DeviceModel) {
        // Single use: `self` is consumed, so the cell is still empty and `set` cannot fail.
        let _ = self.0.set(model);
    }
}

enum Verdict {
    Granted,
    Unauthenticated,
    Stale,
    /// A failed WS-Security token: answered with the SOAP fault.
    Denied,
    /// A failed HTTP Digest attempt: answered with 401 and a fresh challenge.
    DigestFailed,
}

impl OnvifService {
    /// Build the service.
    ///
    /// # Errors
    ///
    /// Returns [`SetupError`] when the system random source is unavailable.
    pub fn new(
        model: DeviceModel, credentials: Credentials, clock: Arc<dyn Clock>,
    ) -> Result<Self, SetupError> {
        let digest = DigestAuth::new(credentials.clone(), REALM, clock.clone());
        Self::from_digest(model, credentials, clock, digest)
    }

    /// A service with no model yet, e.g. while the camera that defines the profiles is absent.
    /// Until the returned publisher supplies one, the Device and Media services answer `503`
    /// and the snapshot endpoint answers `503` after authentication; only the status endpoint
    /// (see [`OnvifService::with_status_source`]) reports anything.
    ///
    /// # Errors
    ///
    /// Returns [`SetupError`] when the system random source is unavailable.
    pub fn pending(
        credentials: Credentials, clock: Arc<dyn Clock>,
    ) -> Result<(Self, ModelPublisher), SetupError> {
        let digest = DigestAuth::new(credentials.clone(), REALM, clock.clone());
        Self::pending_from_digest(credentials, clock, digest)
    }

    fn pending_from_digest(
        credentials: Credentials, clock: Arc<dyn Clock>, digest: Result<DigestAuth, AuthError>,
    ) -> Result<(Self, ModelPublisher), SetupError> {
        let model = Arc::new(OnceLock::new());
        let publisher = ModelPublisher(Arc::clone(&model));
        Ok((
            Self::assemble_with(model, credentials, clock, digest?),
            publisher,
        ))
    }

    /// Like [`OnvifService::new`] with a fixed Digest secret, for deterministic tests.
    #[must_use]
    pub fn with_digest_secret(
        model: DeviceModel, credentials: Credentials, clock: Arc<dyn Clock>, secret: [u8; 32],
    ) -> Self {
        let digest = DigestAuth::with_secret(credentials.clone(), REALM, clock.clone(), secret);
        Self::assemble(model, credentials, clock, digest)
    }

    fn from_digest(
        model: DeviceModel, credentials: Credentials, clock: Arc<dyn Clock>,
        digest: Result<DigestAuth, AuthError>,
    ) -> Result<Self, SetupError> {
        Ok(Self::assemble(model, credentials, clock, digest?))
    }

    fn assemble(
        model: DeviceModel, credentials: Credentials, clock: Arc<dyn Clock>, digest: DigestAuth,
    ) -> Self {
        Self::assemble_with(Arc::new(OnceLock::from(model)), credentials, clock, digest)
    }

    fn assemble_with(
        model: Arc<OnceLock<DeviceModel>>, credentials: Credentials, clock: Arc<dyn Clock>,
        digest: DigestAuth,
    ) -> Self {
        Self {
            model,
            wsse: WsseVerifier::new(credentials, clock.clone(), WsseConfig::default()),
            clock,
            digest,
            snapshot: None,
            status: None,
            limits: SoapLimits::default(),
        }
    }

    /// Enable `GetSnapshotUri` and the snapshot endpoint.
    #[must_use]
    pub fn with_snapshot_source(mut self, source: Arc<dyn SnapshotSource>) -> Self {
        self.snapshot = Some(source);
        self
    }

    /// Enable the authenticated status endpoint, `GET /tethercam/status`.
    #[must_use]
    pub fn with_status_source(mut self, source: Arc<dyn StatusSource>) -> Self {
        self.status = Some(source);
        self
    }

    /// Override the envelope bounds.
    #[must_use]
    pub fn with_limits(mut self, limits: SoapLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The envelope bounds in force.
    #[must_use]
    pub fn limits(&self) -> SoapLimits {
        self.limits
    }

    /// Route and answer one request. Never panics.
    #[must_use]
    pub fn handle(&self, req: &ServiceRequest<'_>) -> ServiceResponse {
        let path = req.target.split_once('?').map_or(req.target, |(p, _)| p);
        match path {
            DEVICE_PATH => self.soap_when_ready(Endpoint::Device, req),
            MEDIA_PATH => self.soap_when_ready(Endpoint::Media, req),
            STATUS_PATH => self.status_request(req),
            _ => match path.strip_prefix(SNAPSHOT_PREFIX) {
                Some(token) => self.snapshot_request(token, req),
                None => ServiceResponse::plain(404),
            },
        }
    }

    fn challenge(&self, stale: bool) -> ServiceResponse {
        let mut resp = ServiceResponse::fault(&Fault::not_authorized());
        resp.status = 401;
        resp.www_authenticate = Some(self.digest.challenge(stale));
        resp
    }

    fn digest_verdict(&self, req: &ServiceRequest<'_>) -> Result<(), ServiceResponse> {
        let Some(header) = req.authorization else {
            return Err(self.challenge(false));
        };
        match self.digest.verify(req.method, req.target, header) {
            Ok(()) => Ok(()),
            Err(AuthError::Stale) => Err(self.challenge(true)),
            Err(_) => Err(self.challenge(false)),
        }
    }

    fn soap_when_ready(&self, endpoint: Endpoint, req: &ServiceRequest<'_>) -> ServiceResponse {
        match self.model.get() {
            Some(model) => self.soap(endpoint, model, req),
            None => ServiceResponse::not_ready(),
        }
    }

    fn status_request(&self, req: &ServiceRequest<'_>) -> ServiceResponse {
        let Some(source) = &self.status else {
            return ServiceResponse::plain(404);
        };
        if req.method != "GET" {
            return ServiceResponse::plain(405);
        }
        if let Err(resp) = self.digest_verdict(req) {
            return resp;
        }
        ServiceResponse {
            status: 200,
            content_type: "application/json",
            body: source.status_json().into_bytes(),
            www_authenticate: None,
        }
    }

    fn snapshot_request(&self, token: &str, req: &ServiceRequest<'_>) -> ServiceResponse {
        let Some(source) = &self.snapshot else {
            return ServiceResponse::plain(404);
        };
        if req.method != "GET" {
            return ServiceResponse::plain(405);
        }
        if let Err(resp) = self.digest_verdict(req) {
            return resp;
        }
        let Some(model) = self.model.get() else {
            return ServiceResponse::not_ready();
        };
        let Some(profile) = model.profile(token) else {
            return ServiceResponse::plain(404);
        };
        match source.snapshot(&profile.token) {
            Ok(jpeg) => ServiceResponse {
                status: 200,
                content_type: "image/jpeg",
                body: jpeg,
                www_authenticate: None,
            },
            Err(SnapshotError) => ServiceResponse::plain(503),
        }
    }

    fn soap(
        &self, endpoint: Endpoint, model: &DeviceModel, req: &ServiceRequest<'_>,
    ) -> ServiceResponse {
        if req.method != "POST" {
            return ServiceResponse::plain(405);
        }
        let env = match Envelope::parse(req.body, &self.limits) {
            Ok(env) => env,
            Err(SoapError::VersionMismatch) => {
                return ServiceResponse::fault(&Fault::version_mismatch());
            }
            Err(SoapError::TooLarge(_)) => return ServiceResponse::plain(413),
            Err(_) => return ServiceResponse::fault(&Fault::malformed()),
        };

        let verdict = self.authenticate(&env, req);
        let authenticated = matches!(verdict, Verdict::Granted);
        if !is_pre_auth(endpoint, &env.operation) {
            match verdict {
                Verdict::Granted => {}
                Verdict::Unauthenticated | Verdict::DigestFailed => return self.challenge(false),
                Verdict::Stale => return self.challenge(true),
                Verdict::Denied => return ServiceResponse::fault(&Fault::not_authorized()),
            }
        }

        let ctx = Context {
            model,
            local: req.local,
            now: self.clock.now_unix(),
            snapshot: self.snapshot.is_some(),
            authenticated,
        };
        let result = match endpoint {
            Endpoint::Device => device::handle(&env.operation, &ctx),
            Endpoint::Media => media::handle(&env.operation, &ctx),
        };
        match result {
            Ok(body) => ServiceResponse::soap(200, envelope(&body)),
            Err(fault) => ServiceResponse::fault(&fault),
        }
    }

    fn authenticate(&self, env: &Envelope, req: &ServiceRequest<'_>) -> Verdict {
        if let Some(security) = env.header_block(NS_WSSE, "Security") {
            return match UsernameToken::from_security(security)
                .and_then(|token| self.wsse.verify(&token))
            {
                Ok(()) => Verdict::Granted,
                Err(_) => Verdict::Denied,
            };
        }
        if let Some(header) = req.authorization {
            return match self.digest.verify(req.method, req.target, header) {
                Ok(()) => Verdict::Granted,
                Err(AuthError::Stale) => Verdict::Stale,
                Err(_) => Verdict::DigestFailed,
            };
        }
        Verdict::Unauthenticated
    }
}

fn is_pre_auth(endpoint: Endpoint, op: &Element) -> bool {
    endpoint == Endpoint::Device
        && op.namespace.as_deref() == Some(NS_DEVICE)
        && matches!(
            op.name.as_str(),
            "GetSystemDateAndTime" | "GetCapabilities" | "GetServices"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onvif::{model::example_model, time::ManualClock};

    #[test]
    fn constructor_propagates_entropy_failure() {
        let credentials = Credentials::new("synthetic-user", "synthetic-password").unwrap();
        let result = OnvifService::from_digest(
            example_model(),
            credentials,
            Arc::new(ManualClock::new(0)),
            Err(AuthError::Entropy),
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "authentication setup failed: random source unavailable"
        );
        let credentials = Credentials::new("synthetic-user", "synthetic-password").unwrap();
        let clock = Arc::new(ManualClock::new(0));
        let digest = DigestAuth::with_secret(credentials.clone(), REALM, clock.clone(), [9; 32]);
        assert!(OnvifService::from_digest(example_model(), credentials, clock, Ok(digest)).is_ok());
    }

    #[test]
    fn pending_constructor_propagates_entropy_failure() {
        let credentials = Credentials::new("synthetic-user", "synthetic-password").unwrap();
        let clock = Arc::new(ManualClock::new(0));
        let error = OnvifService::pending_from_digest(
            credentials.clone(),
            clock.clone(),
            Err(AuthError::Entropy),
        )
        .err()
        .unwrap();
        assert_eq!(
            error.to_string(),
            "authentication setup failed: random source unavailable"
        );
        assert!(OnvifService::pending(credentials, clock).is_ok());
    }

    #[test]
    fn response_debug_redacts_issued_challenge() {
        let service = OnvifService::with_digest_secret(
            example_model(),
            Credentials::new("synthetic-user", "synthetic-password").unwrap(),
            Arc::new(ManualClock::new(1_750_000_000)),
            [9; 32],
        );
        let response = service.challenge(false);
        let challenge = response.www_authenticate.as_ref().unwrap();
        let nonce = challenge
            .split("nonce=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let debug = format!("{response:?}");
        assert!(!debug.contains(nonce));
        assert!(debug.contains("[REDACTED]"));
    }
}
