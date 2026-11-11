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
use std::sync::Arc;

use thiserror::Error;

use super::auth::digest::DigestAuth;
use super::auth::wsse::{NS_WSSE, UsernameToken, WsseConfig, WsseVerifier};
use super::auth::{AuthError, Credentials};
use super::device::{self, DEVICE_PATH};
use super::media::{self, MEDIA_PATH, SNAPSHOT_PREFIX};
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
#[derive(Debug, Clone, Copy)]
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

/// A response to write back.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    model: DeviceModel,
    clock: Arc<dyn Clock>,
    wsse: WsseVerifier,
    digest: DigestAuth,
    snapshot: Option<Arc<dyn SnapshotSource>>,
    limits: SoapLimits,
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
        let digest = DigestAuth::new(credentials.clone(), REALM, clock.clone())?;
        Ok(Self::assemble(model, credentials, clock, digest))
    }

    /// Like [`OnvifService::new`] with a fixed Digest secret, for deterministic tests.
    #[must_use]
    pub fn with_digest_secret(
        model: DeviceModel, credentials: Credentials, clock: Arc<dyn Clock>, secret: [u8; 32],
    ) -> Self {
        let digest = DigestAuth::with_secret(credentials.clone(), REALM, clock.clone(), secret);
        Self::assemble(model, credentials, clock, digest)
    }

    fn assemble(
        model: DeviceModel, credentials: Credentials, clock: Arc<dyn Clock>, digest: DigestAuth,
    ) -> Self {
        Self {
            model,
            wsse: WsseVerifier::new(credentials, clock.clone(), WsseConfig::default()),
            clock,
            digest,
            snapshot: None,
            limits: SoapLimits::default(),
        }
    }

    /// Enable `GetSnapshotUri` and the snapshot endpoint.
    #[must_use]
    pub fn with_snapshot_source(mut self, source: Arc<dyn SnapshotSource>) -> Self {
        self.snapshot = Some(source);
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
            DEVICE_PATH => self.soap(Endpoint::Device, req),
            MEDIA_PATH => self.soap(Endpoint::Media, req),
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
        let Some(profile) = self.model.profile(token) else {
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

    fn soap(&self, endpoint: Endpoint, req: &ServiceRequest<'_>) -> ServiceResponse {
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
            model: &self.model,
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
