//! WS-Discovery message parsing and building.
//!
//! Parsing is total: [`parse_request`] turns arbitrary bytes into a [`Request`] or a
//! [`ParseError`]. Building produces small SOAP 1.2 envelopes by hand; every interpolated value
//! is XML-escaped.
//!
//! References:
//! - [WS-Discovery 1.1](http://docs.oasis-open.org/ws-dd/discovery/1.1/os/wsdd-discovery-1.1-spec-os.html)
//! - [SOAP 1.2 Part 1](https://www.w3.org/TR/soap12-part1/)
//! - [WS-Addressing 1.0 Core](https://www.w3.org/TR/ws-addr-core/)

use super::xml::{self, Element, QName, SyntaxError};
use thiserror::Error;

/// SOAP 1.2 envelope namespace.
pub(super) const NS_SOAP: &str = "http://www.w3.org/2003/05/soap-envelope";
/// ONVIF network (`dn`) namespace, home of `NetworkVideoTransmitter`.
pub(super) const NS_DN: &str = "http://www.onvif.org/ver10/network/wsdl";
/// ONVIF device (`tds`) namespace, home of `Device`.
pub(super) const NS_TDS: &str = "http://www.onvif.org/ver10/device/wsdl";

/// Which WS-Discovery and WS-Addressing generation a message speaks.
///
/// ONVIF devices and clients in the field (including NVRs) use the April 2005 WS-Discovery and
/// August 2004 WS-Addressing namespaces; OASIS WS-Discovery 1.1 uses 2009/01 and WS-Addressing
/// 1.0. Both are accepted, and a reply is always in the dialect of the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// WS-Discovery 2005/04 with WS-Addressing 2004/08, the ONVIF-era dialect.
    Legacy,
    /// WS-Discovery 1.1 (2009/01) with WS-Addressing 1.0.
    Standard,
}

impl Dialect {
    /// Both dialects, ONVIF-era first.
    pub(super) const ALL: [Self; 2] = [Self::Legacy, Self::Standard];

    /// The WS-Discovery namespace.
    pub(super) const fn wsd(self) -> &'static str {
        match self {
            Self::Legacy => "http://schemas.xmlsoap.org/ws/2005/04/discovery",
            Self::Standard => "http://docs.oasis-open.org/ws-dd/ns/discovery/2009/01",
        }
    }

    /// The WS-Addressing namespace.
    pub(super) const fn wsa(self) -> &'static str {
        match self {
            Self::Legacy => "http://schemas.xmlsoap.org/ws/2004/08/addressing",
            Self::Standard => "http://www.w3.org/2005/08/addressing",
        }
    }

    /// `To` address of multicast discovery messages.
    const fn to_discovery(self) -> &'static str {
        match self {
            Self::Legacy => "urn:schemas-xmlsoap-org:ws:2005:04:discovery",
            Self::Standard => "urn:docs-oasis-open-org:ws-dd:ns:discovery:2009:01",
        }
    }

    /// `To` address of unicast replies.
    const fn to_anonymous(self) -> &'static str {
        match self {
            Self::Legacy => "http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous",
            Self::Standard => "http://www.w3.org/2005/08/addressing/anonymous",
        }
    }

    /// Action URI of the message named `name` (`Probe`, `Hello`, ...).
    fn action(self, name: &str) -> String {
        format!("{}/{name}", self.wsd())
    }

    fn from_wsd(ns: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.wsd() == ns)
    }
}

/// Longest `MessageID` accepted, measured after XML escaping (what `RelatesTo` will carry), so
/// it bounds the size of the reply that echoes it.
pub const MAX_MESSAGE_ID: usize = 128;
/// Most entries accepted in a probe's `Types` or `Scopes`.
const MAX_LIST: usize = 16;
/// Longest single scope accepted in a probe.
const MAX_PROBE_SCOPE: usize = 256;

/// Why bytes could not be understood as a discovery request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ParseError {
    /// The XML is invalid or over budget.
    #[error("invalid XML: {0}")]
    Xml(#[from] SyntaxError),
    /// The root is not a SOAP 1.2 `Envelope`.
    #[error("root element is not a SOAP 1.2 `Envelope`")]
    NotEnvelope,
    /// The envelope has no `Header`.
    #[error("envelope has no `Header`")]
    MissingHeader,
    /// The envelope has no `Body`.
    #[error("envelope has no `Body`")]
    MissingBody,
    /// The header has no `Action`.
    #[error("header has no `Action`")]
    MissingAction,
    /// The header has no usable `MessageID`.
    #[error("header has no `MessageID`")]
    MissingMessageId,
    /// The `MessageID` is longer than the limit.
    #[error("escaped `MessageID` is longer than {MAX_MESSAGE_ID} bytes")]
    MessageIdTooLong,
    /// The body is not a `Probe` or `Resolve` (e.g. a peer's `Hello`).
    #[error("body is not a `Probe` or `Resolve`")]
    NotARequest,
    /// The `Action` header does not belong to the body element.
    #[error("`Action` does not match the body element")]
    ActionMismatch,
    /// A list in the probe is longer than the limit or an entry is oversized.
    #[error("`Types` or `Scopes` exceed the allowed size")]
    ListTooLong,
    /// A `Resolve` carries no endpoint reference address.
    #[error("`Resolve` has no `EndpointReference`")]
    MissingEndpoint,
}

/// A parsed `Probe`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRequest {
    /// The request's `MessageID`, echoed as `RelatesTo`.
    pub message_id: String,
    /// The dialect the request spoke; the reply uses it too.
    pub dialect: Dialect,
    /// Requested types; empty matches every target.
    pub types: Vec<QName>,
    /// Requested scopes; empty matches every target.
    pub scopes: Vec<String>,
    /// The `MatchBy` rule on `Scopes`, if given.
    pub match_by: Option<String>,
}

/// A parsed `Resolve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveRequest {
    /// The request's `MessageID`, echoed as `RelatesTo`.
    pub message_id: String,
    /// The dialect the request spoke; the reply uses it too.
    pub dialect: Dialect,
    /// The endpoint reference address being resolved.
    pub endpoint: String,
}

/// A request a target must answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Look for targets by type and scope.
    Probe(ProbeRequest),
    /// Look for a target by endpoint reference.
    Resolve(ResolveRequest),
}

/// Parse a datagram payload as a `Probe` or `Resolve`.
///
/// # Errors
///
/// Returns a [`ParseError`] for any input that is not a well-formed request.
pub fn parse_request(bytes: &[u8]) -> Result<Request, ParseError> {
    let text = std::str::from_utf8(bytes).map_err(|_| SyntaxError::NotUtf8)?;
    let root = xml::parse(text)?;
    if !root.is(NS_SOAP, "Envelope") {
        return Err(ParseError::NotEnvelope);
    }
    let header = root
        .child(NS_SOAP, "Header")
        .ok_or(ParseError::MissingHeader)?;
    let body = root.child(NS_SOAP, "Body").ok_or(ParseError::MissingBody)?;

    let (element, dialect) = body
        .children
        .iter()
        .filter(|c| matches!(c.local.as_str(), "Probe" | "Resolve"))
        .find_map(|c| Some((c, Dialect::from_wsd(c.ns.as_deref()?)?)))
        .ok_or(ParseError::NotARequest)?;
    let action = header_text(header, dialect, "Action").ok_or(ParseError::MissingAction)?;
    if action != dialect.action(&element.local) {
        return Err(ParseError::ActionMismatch);
    }
    let message_id =
        header_text(header, dialect, "MessageID").ok_or(ParseError::MissingMessageId)?;
    if escaped_len(message_id) > MAX_MESSAGE_ID {
        return Err(ParseError::MessageIdTooLong);
    }
    let message_id = message_id.to_owned();

    if element.local == "Probe" {
        parse_probe(element, message_id, dialect).map(Request::Probe)
    } else {
        let endpoint = element
            .child(dialect.wsa(), "EndpointReference")
            .and_then(|e| e.child(dialect.wsa(), "Address"))
            .map(|a| a.text.trim())
            .filter(|a| !a.is_empty())
            .ok_or(ParseError::MissingEndpoint)?;
        Ok(Request::Resolve(ResolveRequest {
            message_id,
            dialect,
            endpoint: endpoint.to_owned(),
        }))
    }
}

fn header_text<'e>(header: &'e Element, dialect: Dialect, local: &str) -> Option<&'e str> {
    header
        .child(dialect.wsa(), local)
        .map(|e| e.text.trim())
        .filter(|t| !t.is_empty())
}

fn parse_probe(
    probe: &Element, message_id: String, dialect: Dialect,
) -> Result<ProbeRequest, ParseError> {
    let mut types = Vec::new();
    if let Some(el) = probe.child(dialect.wsd(), "Types") {
        for token in el.text.split_whitespace() {
            if types.len() >= MAX_LIST {
                return Err(ParseError::ListTooLong);
            }
            types.push(el.resolve_qname(token)?);
        }
    }
    let mut scopes = Vec::new();
    let mut match_by = None;
    if let Some(el) = probe.child(dialect.wsd(), "Scopes") {
        match_by = el.attr("MatchBy").map(str::to_owned);
        for token in el.text.split_whitespace() {
            if scopes.len() >= MAX_LIST || token.len() > MAX_PROBE_SCOPE {
                return Err(ParseError::ListTooLong);
            }
            scopes.push(token.to_owned());
        }
    }
    Ok(ProbeRequest {
        message_id,
        dialect,
        types,
        scopes,
        match_by,
    })
}

/// The `AppSequence` header carried by every outgoing announcement and match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppSequence {
    /// Identifies this run of the responder; must increase across restarts.
    pub instance_id: u32,
    /// Increases by one per outgoing message within an instance.
    pub message_number: u32,
}

/// What a `Hello`, `ProbeMatch` or `ResolveMatch` says about this target.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    /// The stable `urn:uuid:` endpoint reference.
    pub endpoint: &'a str,
    /// Scope URIs, already validated by the caller.
    pub scopes: &'a [String],
    /// Transport address of the Device service on the interface the peer can reach.
    pub xaddr: &'a str,
    /// Bumped when types, scopes or addresses change.
    pub metadata_version: u32,
}

/// Whether a target of the ONVIF types (`dn:NetworkVideoTransmitter`, `tds:Device`) matches the
/// requested types. Every requested type must be one of ours; an empty request matches.
#[must_use]
pub(super) fn types_match(requested: &[QName]) -> bool {
    requested.iter().all(|q| match q.ns.as_deref() {
        Some(NS_DN) => q.local == "NetworkVideoTransmitter",
        Some(NS_TDS) => q.local == "Device",
        _ => false,
    })
}

/// Build a multicast `Hello`.
#[must_use]
pub(super) fn build_hello(
    dialect: Dialect, message_id: &str, seq: AppSequence, target: &Target<'_>,
) -> String {
    let body = format!("<d:Hello>{}</d:Hello>", target_xml(target));
    envelope(dialect, "Hello", message_id, None, seq, &body)
}

/// Build a multicast `Bye`. Discloses only the endpoint reference and metadata version.
#[must_use]
pub(super) fn build_bye(
    dialect: Dialect, message_id: &str, seq: AppSequence, target: &Target<'_>,
) -> String {
    let body = format!(
        "<d:Bye>{}<d:MetadataVersion>{}</d:MetadataVersion></d:Bye>",
        endpoint_xml(target.endpoint),
        target.metadata_version
    );
    envelope(dialect, "Bye", message_id, None, seq, &body)
}

/// Build the unicast `ProbeMatches` answering `relates_to`.
#[must_use]
pub(super) fn build_probe_matches(
    dialect: Dialect, message_id: &str, relates_to: &str, seq: AppSequence, target: &Target<'_>,
) -> String {
    let body = format!(
        "<d:ProbeMatches><d:ProbeMatch>{}</d:ProbeMatch></d:ProbeMatches>",
        target_xml(target)
    );
    envelope(
        dialect,
        "ProbeMatches",
        message_id,
        Some(relates_to),
        seq,
        &body,
    )
}

/// Build the unicast `ResolveMatches` answering `relates_to`.
#[must_use]
pub(super) fn build_resolve_matches(
    dialect: Dialect, message_id: &str, relates_to: &str, seq: AppSequence, target: &Target<'_>,
) -> String {
    let body = format!(
        "<d:ResolveMatches><d:ResolveMatch>{}</d:ResolveMatch></d:ResolveMatches>",
        target_xml(target)
    );
    envelope(
        dialect,
        "ResolveMatches",
        message_id,
        Some(relates_to),
        seq,
        &body,
    )
}

fn endpoint_xml(endpoint: &str) -> String {
    format!(
        "<a:EndpointReference><a:Address>{}</a:Address></a:EndpointReference>",
        escape(endpoint)
    )
}

fn target_xml(target: &Target<'_>) -> String {
    format!(
        "{}<d:Types>dn:NetworkVideoTransmitter tds:Device</d:Types><d:Scopes>{}</d:Scopes><d:XAddrs>{}</d:XAddrs>\
         <d:MetadataVersion>{}</d:MetadataVersion>",
        endpoint_xml(target.endpoint),
        escape(&target.scopes.join(" ")),
        escape(target.xaddr),
        target.metadata_version
    )
}

fn envelope(
    dialect: Dialect, name: &str, message_id: &str, relates_to: Option<&str>, seq: AppSequence,
    body: &str,
) -> String {
    let relates = relates_to
        .map(|r| format!("<a:RelatesTo>{}</a:RelatesTo>", escape(r)))
        .unwrap_or_default();
    let to = if relates_to.is_some() {
        dialect.to_anonymous()
    } else {
        dialect.to_discovery()
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <s:Envelope xmlns:s=\"{NS_SOAP}\" xmlns:a=\"{}\" xmlns:d=\"{}\" \
         xmlns:dn=\"{NS_DN}\" xmlns:tds=\"{NS_TDS}\">\
         <s:Header><a:MessageID>{}</a:MessageID>{relates}<a:To>{to}</a:To>\
         <a:Action>{}</a:Action>\
         <d:AppSequence InstanceId=\"{}\" MessageNumber=\"{}\"/></s:Header>\
         <s:Body>{body}</s:Body></s:Envelope>",
        dialect.wsa(),
        dialect.wsd(),
        escape(message_id),
        dialect.action(name),
        seq.instance_id,
        seq.message_number
    )
}

/// Length in bytes of `s` once passed through [`escape`].
fn escaped_len(s: &str) -> usize {
    s.chars()
        .map(|c| match c {
            '&' => 5,
            '<' | '>' => 4,
            '"' | '\'' => 6,
            c => c.len_utf8(),
        })
        .sum()
}

/// Escape text for use in element content or a double-quoted attribute.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// How a probe's `Scopes` are compared with ours, per `MatchBy`.
///
/// `rfc3986` is the default. Unknown rules never match: a UDP responder has nowhere to send the
/// `MatchingRuleNotSupported` fault without becoming an amplifier.
#[must_use]
pub(super) fn scopes_match(
    dialect: Dialect, match_by: Option<&str>, requested: &[String], ours: &[String],
) -> bool {
    // 2005/04 names the default rule `rfc2396`, 2009/01 names it `rfc3986`. The URI
    // normalization is the same for the scope URIs ONVIF uses.
    let default_rule = match dialect {
        Dialect::Legacy => "rfc2396",
        Dialect::Standard => "rfc3986",
    };
    let rule = match_by.map_or_else(
        || default_rule.to_owned(),
        |uri| {
            uri.strip_prefix(dialect.wsd())
                .and_then(|r| r.strip_prefix('/'))
                .unwrap_or("")
                .to_owned()
        },
    );
    let matcher: fn(&str, &str) -> bool = match (dialect, rule.as_str()) {
        (Dialect::Legacy, "rfc2396") | (Dialect::Standard, "rfc3986") => rfc3986_match,
        (_, "strcmp0") => |r, o| r == o,
        // An unsupported rule never matches, whatever the scope list.
        _ => return false,
    };
    requested.iter().all(|r| ours.iter().any(|o| matcher(r, o)))
}

/// Decode percent-escapes of unreserved characters (RFC 3986 section 6.2.2.2); leave others.
fn normalize_segment(seg: &str) -> String {
    let bytes = seg.as_bytes();
    let mut out = String::with_capacity(seg.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let decoded = (b == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok())
            .filter(|d| d.is_ascii_alphanumeric() || matches!(d, b'-' | b'.' | b'_' | b'~'));
        if let Some(d) = decoded {
            out.push(char::from(d));
            i += 3;
        } else {
            out.push(char::from(b));
            i += 1;
        }
    }
    out
}

/// Split a URI into lowercase `scheme`, lowercase authority and normalized path segments.
/// Empty interior segments are kept; one trailing slash is ignored; `.` and `..` segments are
/// resolved (RFC 3986 section 6.2.2.3).
fn split_uri(uri: &str) -> Option<(String, String, Vec<String>)> {
    if !uri.is_ascii() {
        return None;
    }
    let (scheme, rest) = uri.split_once(':')?;
    let (authority, path) = match rest.strip_prefix("//") {
        Some(after) => after.split_once('/').map_or((after, ""), |(a, p)| (a, p)),
        None => ("", rest),
    };
    let path = path.strip_suffix('/').unwrap_or(path);
    let mut segments: Vec<String> = Vec::new();
    if !path.is_empty() {
        for segment in path.split('/').map(normalize_segment) {
            match segment.as_str() {
                "." => {}
                ".." => {
                    segments.pop();
                }
                _ => segments.push(segment),
            }
        }
    }
    Some((scheme.to_lowercase(), authority.to_lowercase(), segments))
}

/// RFC 3986 matching: scheme and authority equal ignoring case, and the requested path's
/// segments are a prefix of ours.
fn rfc3986_match(requested: &str, ours: &str) -> bool {
    match (split_uri(requested), split_uri(ours)) {
        (Some((rs, ra, rp)), Some((os, oa, op))) => {
            rs == os && ra == oa && rp.len() <= op.len() && rp.iter().zip(&op).all(|(r, o)| r == o)
        }
        _ => false,
    }
}

#[cfg(test)]
pub(super) mod fixtures {
    /// Modeled on the probe an ONVIF discovery tool sends: 2005/04 discovery, 2004/08
    /// addressing, `dn:` types, no scopes.
    pub(in crate::onvif::discovery) const PROBE_ONVIF: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope"
 xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing"
 xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"
 xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
<e:Header>
<w:MessageID>uuid:11111111-2222-3333-4444-555555555555</w:MessageID>
<w:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To>
<w:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action>
</e:Header>
<e:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></e:Body>
</e:Envelope>"#;

    /// Modeled on a generic WS-Discovery client probe: no `Types`, no `Scopes`.
    pub(in crate::onvif::discovery) const PROBE_ANY: &str = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://www.w3.org/2005/08/addressing" xmlns:d="http://docs.oasis-open.org/ws-dd/ns/discovery/2009/01"><s:Header><a:Action>http://docs.oasis-open.org/ws-dd/ns/discovery/2009/01/Probe</a:Action><a:MessageID>urn:uuid:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee</a:MessageID><a:To>urn:docs-oasis-open-org:ws-dd:ns:discovery:2009:01</a:To></s:Header><s:Body><d:Probe/></s:Body></s:Envelope>"#;

    /// Resolve for a synthetic endpoint.
    pub(in crate::onvif::discovery) const RESOLVE: &str = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://www.w3.org/2005/08/addressing" xmlns:d="http://docs.oasis-open.org/ws-dd/ns/discovery/2009/01"><s:Header><a:Action>http://docs.oasis-open.org/ws-dd/ns/discovery/2009/01/Resolve</a:Action><a:MessageID>urn:uuid:99999999-bbbb-cccc-dddd-eeeeeeeeeeee</a:MessageID></s:Header><s:Body><d:Resolve><a:EndpointReference><a:Address>urn:uuid:01234567-89ab-4def-8123-456789abcdef</a:Address></a:EndpointReference></d:Resolve></s:Body></s:Envelope>"#;
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use proptest::prelude::*;

    fn seq() -> AppSequence {
        AppSequence {
            instance_id: 7,
            message_number: 3,
        }
    }

    fn scopes() -> Vec<String> {
        vec![
            "onvif://www.onvif.org/type/video_encoder".to_owned(),
            "onvif://www.onvif.org/Profile/Streaming".to_owned(),
            "onvif://www.onvif.org/name/Test%20Cam".to_owned(),
        ]
    }

    fn target(scopes: &[String]) -> Target<'_> {
        Target {
            endpoint: "urn:uuid:01234567-89ab-4def-8123-456789abcdef",
            scopes,
            xaddr: "http://192.0.2.10:8080/onvif/device_service",
            metadata_version: 1,
        }
    }

    #[test]
    fn parses_onvif_probe() {
        assert_eq!(
            parse_request(PROBE_ONVIF.as_bytes()).unwrap(),
            Request::Probe(ProbeRequest {
                message_id: "uuid:11111111-2222-3333-4444-555555555555".to_owned(),
                dialect: Dialect::Legacy,
                types: vec![QName {
                    ns: Some(NS_DN.to_owned()),
                    local: "NetworkVideoTransmitter".to_owned()
                }],
                scopes: vec![],
                match_by: None,
            })
        );
    }

    #[test]
    fn detects_the_request_dialect() {
        // A legacy body with a 2009/01 action is not a valid message in either dialect.
        let mixed = PROBE_ONVIF.replace(
            "http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe<",
            "http://docs.oasis-open.org/ws-dd/ns/discovery/2009/01/Probe<",
        );
        assert_eq!(
            parse_request(mixed.as_bytes()).unwrap_err(),
            ParseError::ActionMismatch
        );
    }

    #[test]
    fn legacy_replies_use_legacy_namespaces() {
        let scopes = scopes();
        let msg = build_probe_matches(
            Dialect::Legacy,
            "urn:uuid:m",
            "uuid:r",
            seq(),
            &target(&scopes),
        );
        for want in [
            "xmlns:a=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\"",
            "xmlns:d=\"http://schemas.xmlsoap.org/ws/2005/04/discovery\"",
            "<a:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches</a:Action>",
            "<a:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</a:To>",
        ] {
            assert!(msg.contains(want), "{want}");
        }
        let hello = build_hello(Dialect::Legacy, "urn:uuid:m", seq(), &target(&scopes));
        assert!(hello.contains("<a:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</a:To>"));
    }

    #[test]
    fn parses_untyped_probe_and_resolve() {
        assert_eq!(
            parse_request(PROBE_ANY.as_bytes()).unwrap(),
            Request::Probe(ProbeRequest {
                message_id: "urn:uuid:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
                dialect: Dialect::Standard,
                types: vec![],
                scopes: vec![],
                match_by: None,
            })
        );
        assert_eq!(
            parse_request(RESOLVE.as_bytes()).unwrap(),
            Request::Resolve(ResolveRequest {
                message_id: "urn:uuid:99999999-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
                dialect: Dialect::Standard,
                endpoint: "urn:uuid:01234567-89ab-4def-8123-456789abcdef".to_owned(),
            })
        );
    }

    #[test]
    fn matches_scopes_without_an_authority() {
        assert!(rfc3986_match("urn:test", "urn:test/path"));
    }

    #[test]
    fn rejects_missing_headers_wrong_namespaces_and_long_type_lists() {
        for (doc, err) in [
            (PROBE_ANY.replace("<a:Action>http://docs.oasis-open.org/ws-dd/ns/discovery/2009/01/Probe</a:Action>", ""), ParseError::MissingAction),
            (
                PROBE_ANY
                    .replace("<s:Header>", "<s:Other>")
                    .replace("</s:Header>", "</s:Other>"),
                ParseError::MissingHeader,
            ),
            (
                PROBE_ANY
                    .replace("<s:Body>", "<s:Other>")
                    .replace("</s:Body>", "</s:Other>"),
                ParseError::MissingBody,
            ),
            (
                PROBE_ANY
                    .replace("<d:Probe/>", "<Probe xmlns=''/ >")
                    .replace("/ >", "/>"),
                ParseError::NotARequest,
            ),
            (
                PROBE_ANY.replace("<d:Probe/>", "<Probe xmlns='urn:unknown'/>"),
                ParseError::NotARequest,
            ),
            (
                PROBE_ANY.replace(
                    "<d:Probe/>",
                    &format!(
                        "<d:Probe><d:Types>{}</d:Types></d:Probe>",
                        "d:Thing ".repeat(MAX_LIST + 1)
                    ),
                ),
                ParseError::ListTooLong,
            ),
        ] {
            assert_eq!(parse_request(doc.as_bytes()).unwrap_err(), err);
        }
    }

    #[test]
    fn rejects_structural_problems() {
        let cases = [
            (PROBE_ANY.replace("Envelope", "Other"), ParseError::NotEnvelope),
            (
                PROBE_ANY.replace("<a:MessageID>urn:uuid:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee</a:MessageID>", ""),
                ParseError::MissingMessageId,
            ),
            (
                PROBE_ANY.replace("2009/01/Probe<", "2009/01/Resolve<"),
                ParseError::ActionMismatch,
            ),
            (
                PROBE_ANY.replace("d:Probe/", "d:Hello/"),
                ParseError::NotARequest,
            ),
            (
                RESOLVE.replace("<a:EndpointReference><a:Address>urn:uuid:01234567-89ab-4def-8123-456789abcdef</a:Address></a:EndpointReference>", ""),
                ParseError::MissingEndpoint,
            ),
            (
                PROBE_ANY.replace("urn:uuid:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", &"x".repeat(200)),
                ParseError::MessageIdTooLong,
            ),
        ];
        for (doc, want) in cases {
            assert_eq!(parse_request(doc.as_bytes()).unwrap_err(), want);
        }
        assert_eq!(
            parse_request(&[0xff, 0xfe]),
            Err(ParseError::Xml(SyntaxError::NotUtf8))
        );
    }

    #[test]
    fn message_id_bound_applies_after_escaping() {
        let with_id =
            |id: &str| PROBE_ANY.replace("urn:uuid:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", id);
        // In the document `&amp;` decodes to `&`; 25 of them escape to 125 bytes, plus 3 more:
        // exactly the 128-byte bound.
        let at_bound = format!("{}xxx", "&amp;".repeat(25));
        assert_eq!(escaped_len("&"), 5);
        assert_eq!(escaped_len("<>\"'"), 4 + 4 + 6 + 6);
        assert!(parse_request(with_id(&at_bound).as_bytes()).is_ok());
        let over = format!("{}xxxx", "&amp;".repeat(25));
        assert_eq!(
            parse_request(with_id(&over).as_bytes()).unwrap_err(),
            ParseError::MessageIdTooLong
        );
        // Short in raw bytes, long once escaped.
        let heavy = "&apos;".repeat(25);
        assert_eq!(
            parse_request(with_id(&heavy).as_bytes()).unwrap_err(),
            ParseError::MessageIdTooLong
        );
    }

    #[test]
    fn rejects_unbound_type_prefix_and_long_lists() {
        let doc = PROBE_ANY.replace(
            "<d:Probe/>",
            "<d:Probe><d:Types>zz:Device</d:Types></d:Probe>",
        );
        assert_eq!(
            parse_request(doc.as_bytes()).unwrap_err(),
            ParseError::Xml(SyntaxError::UnboundPrefix)
        );
        let many = "a ".repeat(40);
        let doc = PROBE_ANY.replace(
            "<d:Probe/>",
            &format!("<d:Probe><d:Scopes>{many}</d:Scopes></d:Probe>"),
        );
        assert_eq!(
            parse_request(doc.as_bytes()).unwrap_err(),
            ParseError::ListTooLong
        );
    }

    #[test]
    fn type_matching() {
        let q = |ns: &str, l: &str| QName {
            ns: Some(ns.to_owned()),
            local: l.to_owned(),
        };
        assert!(types_match(&[]));
        assert!(types_match(&[
            q(NS_DN, "NetworkVideoTransmitter"),
            q(NS_TDS, "Device")
        ]));
        assert!(!types_match(&[q(NS_DN, "NetworkVideoDisplay")]));
        assert!(!types_match(&[q("urn:other", "Device")]));
        assert!(!types_match(&[QName {
            ns: None,
            local: "Device".to_owned()
        }]));
    }

    #[test]
    fn scope_matching_rfc3986_and_strcmp0() {
        let ours = scopes();
        let r = |s: &str| vec![s.to_owned()];
        assert!(scopes_match(Dialect::Standard, None, &[], &ours));
        assert!(scopes_match(
            Dialect::Standard,
            None,
            &r("onvif://www.onvif.org"),
            &ours
        ));
        assert!(scopes_match(
            Dialect::Standard,
            None,
            &r("ONVIF://WWW.ONVIF.ORG/Profile"),
            &ours
        ));
        assert!(scopes_match(
            Dialect::Standard,
            None,
            &r("onvif://www.onvif.org/Profile/Streaming/"),
            &ours
        ));
        assert!(!scopes_match(
            Dialect::Standard,
            None,
            &r("onvif://www.onvif.org/Profile/Stream"),
            &ours
        ));
        assert!(!scopes_match(
            Dialect::Standard,
            None,
            &r("onvif://www.onvif.org/Profile/Streaming/x"),
            &ours
        ));
        assert!(!scopes_match(
            Dialect::Standard,
            None,
            &r("onvif://other.example/Profile"),
            &ours
        ));
        assert!(
            !scopes_match(Dialect::Standard, Some("urn:unknown"), &[], &ours),
            "unknown rule never matches, even with no scopes"
        );
        assert!(!scopes_match(
            Dialect::Standard,
            None,
            &r("not a uri"),
            &ours
        ));
        assert!(!scopes_match(
            Dialect::Standard,
            None,
            &r("onvif://www.onvif.org/é"),
            &ours
        ));
        assert!(!scopes_match(
            Dialect::Standard,
            None,
            &r("onvif://www.onvif.org"),
            &r("onvif://www.onvif.org/é")
        ));
        assert!(scopes_match(
            Dialect::Standard,
            None,
            &r("onvif://www.onvif.org/Profile/%53treaming"),
            &ours
        ));
        assert!(scopes_match(
            Dialect::Standard,
            None,
            &r("onvif://www.onvif.org/x/../Profile/./Streaming"),
            &ours
        ));
        assert!(!scopes_match(
            Dialect::Standard,
            None,
            &r("onvif://www.onvif.org//Profile"),
            &ours
        ));
        let strcmp0 = Some("http://docs.oasis-open.org/ws-dd/ns/discovery/2009/01/strcmp0");
        assert!(scopes_match(
            Dialect::Standard,
            strcmp0,
            &r("onvif://www.onvif.org/Profile/Streaming"),
            &ours
        ));
        assert!(!scopes_match(
            Dialect::Standard,
            strcmp0,
            &r("onvif://www.onvif.org/Profile"),
            &ours
        ));
        assert!(!scopes_match(
            Dialect::Standard,
            Some("urn:unknown"),
            &r("onvif://www.onvif.org"),
            &ours
        ));
    }

    #[test]
    fn legacy_dialect_uses_its_own_match_rules() {
        let ours = scopes();
        let r = |s: &str| vec![s.to_owned()];
        let rule = |name: &str| {
            Some(format!(
                "http://schemas.xmlsoap.org/ws/2005/04/discovery/{name}"
            ))
        };
        let l = Dialect::Legacy;
        assert!(scopes_match(
            l,
            None,
            &r("onvif://www.onvif.org/Profile"),
            &ours
        ));
        assert!(scopes_match(
            l,
            rule("rfc2396").as_deref(),
            &r("onvif://www.onvif.org/Profile"),
            &ours
        ));
        assert!(scopes_match(
            l,
            rule("strcmp0").as_deref(),
            &r("onvif://www.onvif.org/Profile/Streaming"),
            &ours
        ));
        assert!(!scopes_match(
            l,
            rule("strcmp0").as_deref(),
            &r("onvif://www.onvif.org/Profile"),
            &ours
        ));
        // 2009/01 rule URIs do not belong to the 2005/04 dialect, nor the reverse.
        let std_rule = Some("http://docs.oasis-open.org/ws-dd/ns/discovery/2009/01/rfc3986");
        assert!(!scopes_match(
            l,
            std_rule,
            &r("onvif://www.onvif.org/Profile"),
            &ours
        ));
        assert!(!scopes_match(
            l,
            rule("ldap").as_deref(),
            &r("onvif://www.onvif.org/Profile"),
            &ours
        ));
    }

    #[test]
    fn built_messages_round_trip_through_the_xml_reader() {
        let scopes = scopes();
        let t = target(&scopes);
        for d in Dialect::ALL {
            for (msg, action, relates) in [
                (build_hello(d, "urn:uuid:m1", seq(), &t), "Hello", false),
                (build_bye(d, "urn:uuid:m2", seq(), &t), "Bye", false),
                (
                    build_probe_matches(d, "urn:uuid:m3", "urn:uuid:req", seq(), &t),
                    "ProbeMatches",
                    true,
                ),
                (
                    build_resolve_matches(d, "urn:uuid:m4", "urn:uuid:req", seq(), &t),
                    "ResolveMatches",
                    true,
                ),
            ] {
                let root = xml::parse(&msg).unwrap();
                let header = root.child(NS_SOAP, "Header").unwrap();
                assert!(header_text(header, d, "Action").unwrap().ends_with(action));
                assert_eq!(header.child(d.wsa(), "RelatesTo").is_some(), relates);
                let seq_el = header.child(d.wsd(), "AppSequence").unwrap();
                assert_eq!(seq_el.attr("InstanceId"), Some("7"));
                assert_eq!(seq_el.attr("MessageNumber"), Some("3"));
                assert_ne!(msg.contains("192.0.2.10"), action == "Bye");
            }
        }
    }

    #[test]
    fn golden_probe_matches() {
        let scopes = scopes();
        let msg = build_probe_matches(
            Dialect::Standard,
            "urn:uuid:m3",
            "uuid:req",
            seq(),
            &target(&scopes),
        );
        let want = concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
            "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" ",
            "xmlns:a=\"http://www.w3.org/2005/08/addressing\" ",
            "xmlns:d=\"http://docs.oasis-open.org/ws-dd/ns/discovery/2009/01\" ",
            "xmlns:dn=\"http://www.onvif.org/ver10/network/wsdl\" ",
            "xmlns:tds=\"http://www.onvif.org/ver10/device/wsdl\">",
            "<s:Header><a:MessageID>urn:uuid:m3</a:MessageID>",
            "<a:RelatesTo>uuid:req</a:RelatesTo>",
            "<a:To>http://www.w3.org/2005/08/addressing/anonymous</a:To>",
            "<a:Action>http://docs.oasis-open.org/ws-dd/ns/discovery/2009/01/ProbeMatches</a:Action>",
            "<d:AppSequence InstanceId=\"7\" MessageNumber=\"3\"/></s:Header>",
            "<s:Body><d:ProbeMatches><d:ProbeMatch>",
            "<a:EndpointReference><a:Address>urn:uuid:01234567-89ab-4def-8123-456789abcdef</a:Address></a:EndpointReference>",
            "<d:Types>dn:NetworkVideoTransmitter tds:Device</d:Types>",
            "<d:Scopes>onvif://www.onvif.org/type/video_encoder onvif://www.onvif.org/Profile/Streaming onvif://www.onvif.org/name/Test%20Cam</d:Scopes>",
            "<d:XAddrs>http://192.0.2.10:8080/onvif/device_service</d:XAddrs>",
            "<d:MetadataVersion>1</d:MetadataVersion>",
            "</d:ProbeMatch></d:ProbeMatches></s:Body></s:Envelope>",
        );
        assert_eq!(msg, want);
    }

    #[test]
    fn escapes_interpolated_values() {
        let scopes = vec!["a<b&c".to_owned()];
        let msg = build_probe_matches(
            Dialect::Legacy,
            "urn:uuid:m",
            "<x>\"'",
            seq(),
            &target(&scopes),
        );
        assert!(xml::parse(&msg).is_ok());
        assert!(msg.contains("a&lt;b&amp;c"));
        assert!(msg.contains("&lt;x&gt;&quot;&apos;"));
    }

    proptest! {
        #[test]
        fn parse_request_never_panics_on_arbitrary_bytes(
            bytes in proptest::collection::vec(any::<u8>(), 0..2048)
        ) {
            let _ = parse_request(&bytes);
        }

        #[test]
        fn parse_request_never_panics_on_mutated_probes(
            at in 0usize..PROBE_ONVIF.len(),
            cut in 0usize..64,
            byte in any::<u8>(),
        ) {
            let mut bytes = PROBE_ONVIF.as_bytes().to_vec();
            if let Some(b) = bytes.get_mut(at) { *b = byte; }
            bytes.truncate(bytes.len().saturating_sub(cut));
            let _ = parse_request(&bytes);
        }

        #[test]
        fn scope_matching_never_panics(a in ".{0,64}", b in ".{0,64}") {
            let _ = scopes_match(Dialect::Standard, None, &[a], &[b]);
        }
    }
}
