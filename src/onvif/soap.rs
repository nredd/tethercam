//! SOAP 1.2 envelope parsing and response building.
//!
//! Parsing is total and bounded: input size, element depth and node count are capped, and
//! any DTD (so any entity declaration, internal or external) is rejected before it is
//! processed. Only the SOAP 1.2 namespace is accepted; SOAP 1.1 gets a `VersionMismatch`.
//!
//! References:
//! - [SOAP 1.2 Part 1](https://www.w3.org/TR/soap12-part1/)
//! - [ONVIF Core, SOAP and faults](https://www.onvif.org/specs/core/ONVIF-Core-Specification.pdf)

use std::borrow::Cow;

use roxmltree::{Document, Node, ParsingOptions};
use thiserror::Error;

/// SOAP 1.2 envelope namespace.
pub const NS_SOAP12: &str = "http://www.w3.org/2003/05/soap-envelope";
/// SOAP 1.1 envelope namespace, recognised only to answer `VersionMismatch`.
pub const NS_SOAP11: &str = "http://schemas.xmlsoap.org/soap/envelope/";
/// ONVIF Device service namespace.
pub const NS_DEVICE: &str = "http://www.onvif.org/ver10/device/wsdl";
/// ONVIF Media service namespace.
pub const NS_MEDIA: &str = "http://www.onvif.org/ver10/media/wsdl";
/// ONVIF common schema namespace.
pub const NS_SCHEMA: &str = "http://www.onvif.org/ver10/schema";
/// ONVIF error namespace.
pub const NS_ERROR: &str = "http://www.onvif.org/ver10/error";

/// Bounds applied to untrusted envelopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoapLimits {
    /// Maximum input size in bytes.
    pub max_bytes: usize,
    /// Maximum element nesting depth, counting the envelope as depth 1.
    pub max_depth: usize,
    /// Maximum number of XML nodes.
    pub max_nodes: u32,
}

impl Default for SoapLimits {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024,
            max_depth: 16,
            max_nodes: 2048,
        }
    }
}

/// Why an envelope was rejected.
#[derive(Debug, Error)]
pub enum SoapError {
    /// Input exceeds [`SoapLimits::max_bytes`].
    #[error("envelope larger than {0} bytes")]
    TooLarge(usize),
    /// Input is not UTF-8.
    #[error("envelope is not UTF-8")]
    NotUtf8,
    /// Elements nest deeper than [`SoapLimits::max_depth`].
    #[error("envelope nests deeper than {0} elements")]
    TooDeep(usize),
    /// The document has a DTD or entity declaration, or exceeds the node limit, or is not
    /// well-formed.
    #[error("malformed or forbidden XML: {0}")]
    Xml(#[from] roxmltree::Error),
    /// The root is a SOAP 1.1 envelope.
    #[error("SOAP 1.1 envelope")]
    VersionMismatch,
    /// The root is not a SOAP 1.2 `Envelope`.
    #[error("root element is not a SOAP 1.2 Envelope")]
    NotEnvelope,
    /// Envelope children are not `Header?` then `Body`.
    #[error("envelope structure invalid")]
    Structure,
    /// `Body` does not contain exactly one element.
    #[error("body must contain exactly one element")]
    BodyElements,
}

/// An owned, namespace-resolved XML element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    /// Namespace URI, if any.
    pub namespace: Option<String>,
    /// Local name.
    pub name: String,
    /// Concatenated direct text children.
    pub text: String,
    /// Attributes as (local name, value).
    pub attributes: Vec<(String, String)>,
    /// Child elements in order.
    pub children: Vec<Element>,
}

impl Element {
    /// First child with this local name, ignoring namespace.
    #[must_use]
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    /// First child with this namespace and local name.
    #[must_use]
    pub fn child_ns(&self, namespace: &str, name: &str) -> Option<&Element> {
        self.children
            .iter()
            .find(|c| c.name == name && c.namespace.as_deref() == Some(namespace))
    }

    /// Trimmed text of the first child with this local name.
    #[must_use]
    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.trim())
    }

    /// Value of an attribute by local name.
    #[must_use]
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Whether this element is `{namespace}name`.
    #[must_use]
    pub fn is(&self, namespace: &str, name: &str) -> bool {
        self.name == name && self.namespace.as_deref() == Some(namespace)
    }
}

/// A parsed SOAP envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// Children of the SOAP `Header`, in order.
    pub header: Vec<Element>,
    /// The single element inside `Body`: the operation.
    pub operation: Element,
}

impl Envelope {
    /// Parse `bytes` as a SOAP 1.2 envelope under `limits`.
    ///
    /// Total: returns an error for every malformed or hostile input and never panics.
    ///
    /// # Errors
    ///
    /// Returns [`SoapError`] when the input is oversized, not UTF-8, not well-formed, has a
    /// DTD, nests too deeply, or is not a SOAP 1.2 envelope with one body element.
    pub fn parse(bytes: &[u8], limits: &SoapLimits) -> Result<Self, SoapError> {
        if bytes.len() > limits.max_bytes {
            return Err(SoapError::TooLarge(limits.max_bytes));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| SoapError::NotUtf8)?;
        let options = ParsingOptions {
            allow_dtd: false,
            nodes_limit: limits.max_nodes,
            ..ParsingOptions::default()
        };
        let doc = Document::parse_with_options(text, options)?;

        let too_deep = doc
            .descendants()
            .any(|n| n.is_element() && n.ancestors().count() > limits.max_depth.saturating_add(1));
        if too_deep {
            return Err(SoapError::TooDeep(limits.max_depth));
        }

        let root = doc.root_element();
        let tag = root.tag_name();
        if tag.name() != "Envelope" {
            return Err(SoapError::NotEnvelope);
        }
        match tag.namespace() {
            Some(NS_SOAP12) => {}
            Some(NS_SOAP11) => return Err(SoapError::VersionMismatch),
            _ => return Err(SoapError::NotEnvelope),
        }

        let mut parts = root.children().filter(Node::is_element);
        let first = parts.next().ok_or(SoapError::Structure)?;
        let (header, body) = if is_soap(first, "Header") {
            (Some(first), parts.next().ok_or(SoapError::Structure)?)
        } else {
            (None, first)
        };
        if !is_soap(body, "Body") || parts.next().is_some() {
            return Err(SoapError::Structure);
        }

        let header = header
            .map(|h| h.children().filter(Node::is_element).map(convert).collect())
            .unwrap_or_default();
        let mut ops = body.children().filter(Node::is_element);
        let operation = ops.next().ok_or(SoapError::BodyElements)?;
        if ops.next().is_some() {
            return Err(SoapError::BodyElements);
        }
        Ok(Self {
            header,
            operation: convert(operation),
        })
    }

    /// First header block with this namespace and local name.
    #[must_use]
    pub fn header_block(&self, namespace: &str, name: &str) -> Option<&Element> {
        self.header.iter().find(|e| e.is(namespace, name))
    }
}

fn is_soap(node: Node<'_, '_>, name: &str) -> bool {
    let tag = node.tag_name();
    tag.name() == name && tag.namespace() == Some(NS_SOAP12)
}

/// Convert a roxmltree element subtree. Depth was already bounded by the caller.
fn convert(node: Node<'_, '_>) -> Element {
    let text = node
        .children()
        .filter(Node::is_text)
        .filter_map(|t| t.text())
        .collect::<String>();
    Element {
        namespace: node.tag_name().namespace().map(str::to_owned),
        name: node.tag_name().name().to_owned(),
        text,
        attributes: node
            .attributes()
            .map(|a| (a.name().to_owned(), a.value().to_owned()))
            .collect(),
        children: node
            .children()
            .filter(Node::is_element)
            .map(convert)
            .collect(),
    }
}

/// Escape text or attribute content for XML output.
#[must_use]
pub fn escape(s: &str) -> Cow<'_, str> {
    if !s.contains(['&', '<', '>', '"', '\'']) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
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
    Cow::Owned(out)
}

/// `<tag>escaped text</tag>`.
#[must_use]
pub fn text_element(tag: &str, text: &str) -> String {
    format!("<{tag}>{}</{tag}>", escape(text))
}

/// Wrap `body` (already serialized XML) in a SOAP 1.2 envelope declaring the ONVIF prefixes
/// `tds`, `trt`, `tt` and `ter`.
#[must_use]
pub fn envelope(body: &str) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<s:Envelope xmlns:s="{soap}" xmlns:tds="{tds}" xmlns:trt="{trt}" "#,
            r#"xmlns:tt="{tt}" xmlns:ter="{ter}"><s:Body>{body}</s:Body></s:Envelope>"#
        ),
        soap = NS_SOAP12,
        tds = NS_DEVICE,
        trt = NS_MEDIA,
        tt = NS_SCHEMA,
        ter = NS_ERROR,
        body = body,
    )
}

/// SOAP fault `Code/Value`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultCode {
    /// The request was at fault.
    Sender,
    /// The service was at fault.
    Receiver,
    /// Unsupported SOAP version.
    VersionMismatch,
}

impl FaultCode {
    fn value(self) -> &'static str {
        match self {
            Self::Sender => "s:Sender",
            Self::Receiver => "s:Receiver",
            Self::VersionMismatch => "s:VersionMismatch",
        }
    }
}

/// A SOAP 1.2 fault with ONVIF subcodes.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{reason}")]
pub struct Fault {
    /// Top-level code.
    pub code: FaultCode,
    /// Nested subcodes, outermost first, as prefixed `QName`s (`ter:...`).
    pub subcodes: Vec<&'static str>,
    /// Human-readable reason. Must not contain secrets.
    pub reason: String,
}

impl Fault {
    /// `ter:NotAuthorized`: authentication failed or was absent.
    #[must_use]
    pub fn not_authorized() -> Self {
        Self::sender(&["ter:NotAuthorized"], "Sender not authorized")
    }

    /// `s:Receiver` / `ter:ActionNotSupported`: unknown or unimplemented operation.
    #[must_use]
    pub fn action_not_supported() -> Self {
        Self {
            code: FaultCode::Receiver,
            subcodes: vec!["ter:ActionNotSupported"],
            reason: "Optional action not implemented".to_owned(),
        }
    }

    /// `ter:InvalidArgVal` with a more specific subcode, e.g. `ter:NoProfile`.
    #[must_use]
    pub fn invalid_arg(subcode: &'static str, reason: &str) -> Self {
        Self::sender(&["ter:InvalidArgVal", subcode], reason)
    }

    /// A plain sender fault for requests that could not be parsed.
    #[must_use]
    pub fn malformed() -> Self {
        Self::sender(&[], "Malformed request")
    }

    /// `s:VersionMismatch`.
    #[must_use]
    pub fn version_mismatch() -> Self {
        Self {
            code: FaultCode::VersionMismatch,
            subcodes: Vec::new(),
            reason: "Only SOAP 1.2 is supported".to_owned(),
        }
    }

    /// `s:Receiver`: an internal failure.
    #[must_use]
    pub fn internal() -> Self {
        Self {
            code: FaultCode::Receiver,
            subcodes: Vec::new(),
            reason: "Internal error".to_owned(),
        }
    }

    fn sender(subcodes: &[&'static str], reason: &str) -> Self {
        Self {
            code: FaultCode::Sender,
            subcodes: subcodes.to_vec(),
            reason: reason.to_owned(),
        }
    }

    /// The HTTP status that carries this fault: 500 for receiver faults, 400 otherwise.
    #[must_use]
    pub fn http_status(&self) -> u16 {
        match self.code {
            FaultCode::Receiver => 500,
            FaultCode::Sender | FaultCode::VersionMismatch => 400,
        }
    }

    /// Serialize as a complete SOAP envelope.
    #[must_use]
    pub fn to_envelope(&self) -> String {
        let subcode = self
            .subcodes
            .iter()
            .rev()
            .fold(String::new(), |inner, code| {
                format!("<s:Subcode><s:Value>{code}</s:Value>{inner}</s:Subcode>")
            });
        envelope(&format!(
            concat!(
                "<s:Fault><s:Code><s:Value>{value}</s:Value>{subcode}</s:Code>",
                r#"<s:Reason><s:Text xml:lang="en">{reason}</s:Text></s:Reason></s:Fault>"#
            ),
            value = self.code.value(),
            subcode = subcode,
            reason = escape(&self.reason),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const LIMITS: SoapLimits = SoapLimits {
        max_bytes: 64 * 1024,
        max_depth: 16,
        max_nodes: 2048,
    };

    fn wrap(body: &str) -> String {
        format!(
            r#"<s:Envelope xmlns:s="{NS_SOAP12}" xmlns:tds="{NS_DEVICE}"><s:Body>{body}</s:Body></s:Envelope>"#
        )
    }

    #[test]
    fn parses_operation_and_header() {
        let xml = format!(
            r#"<s:Envelope xmlns:s="{NS_SOAP12}" xmlns:tds="{NS_DEVICE}"><s:Header><a:X xmlns:a="urn:a"/></s:Header><s:Body><tds:GetHostname/></s:Body></s:Envelope>"#
        );
        let env = Envelope::parse(xml.as_bytes(), &LIMITS).unwrap();
        assert!(env.operation.is(NS_DEVICE, "GetHostname"));
        assert!(env.header_block("urn:a", "X").is_some());
    }

    #[test]
    fn namespace_prefixes_do_not_matter() {
        let xml = format!(
            r#"<e:Envelope xmlns:e="{NS_SOAP12}"><e:Body><x:GetHostname xmlns:x="{NS_DEVICE}"/></e:Body></e:Envelope>"#
        );
        let env = Envelope::parse(xml.as_bytes(), &LIMITS).unwrap();
        assert!(env.operation.is(NS_DEVICE, "GetHostname"));
    }

    #[test]
    fn rejects_soap11() {
        let xml =
            format!(r#"<s:Envelope xmlns:s="{NS_SOAP11}"><s:Body><a/></s:Body></s:Envelope>"#);
        assert!(matches!(
            Envelope::parse(xml.as_bytes(), &LIMITS),
            Err(SoapError::VersionMismatch)
        ));
    }

    #[test]
    fn rejects_structure_violations() {
        for xml in [
            "<root/>".to_owned(),
            format!(r#"<s:Envelope xmlns:s="{NS_SOAP12}"/>"#),
            format!(r#"<s:Envelope xmlns:s="{NS_SOAP12}"><s:Header/></s:Envelope>"#),
            wrap(""),
            wrap("<a/><b/>"),
            format!(
                r#"<s:Envelope xmlns:s="{NS_SOAP12}"><s:Body><a/></s:Body><s:Body><a/></s:Body></s:Envelope>"#
            ),
        ] {
            assert!(Envelope::parse(xml.as_bytes(), &LIMITS).is_err(), "{xml}");
        }
    }

    #[test]
    fn rejects_entities_and_dtd() {
        let laughs = format!(
            r#"<?xml version="1.0"?><!DOCTYPE l [<!ENTITY a "aaaaaaaaaa"><!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;">]><s:Envelope xmlns:s="{NS_SOAP12}"><s:Body><x>&b;</x></s:Body></s:Envelope>"#
        );
        assert!(matches!(
            Envelope::parse(laughs.as_bytes(), &LIMITS),
            Err(SoapError::Xml(_))
        ));
        let xxe = format!(
            r#"<?xml version="1.0"?><!DOCTYPE l [<!ENTITY x SYSTEM "file:///etc/passwd">]><s:Envelope xmlns:s="{NS_SOAP12}"><s:Body><x>&x;</x></s:Body></s:Envelope>"#
        );
        assert!(matches!(
            Envelope::parse(xxe.as_bytes(), &LIMITS),
            Err(SoapError::Xml(_))
        ));
    }

    #[test]
    fn rejects_deep_nesting() {
        let depth = 200;
        let inner = format!("{}{}", "<a>".repeat(depth), "</a>".repeat(depth));
        let xml = wrap(&inner);
        assert!(matches!(
            Envelope::parse(xml.as_bytes(), &LIMITS),
            Err(SoapError::TooDeep(16))
        ));
        let thousands = wrap(&format!("{}{}", "<a>".repeat(5000), "</a>".repeat(5000)));
        let big = SoapLimits {
            max_bytes: usize::MAX,
            ..LIMITS
        };
        assert!(Envelope::parse(thousands.as_bytes(), &big).is_err());
    }

    #[test]
    fn rejects_oversized_input_and_attributes() {
        let attr = "x".repeat(70_000);
        let xml = wrap(&format!(r#"<a v="{attr}"/>"#));
        assert!(matches!(
            Envelope::parse(xml.as_bytes(), &LIMITS),
            Err(SoapError::TooLarge(_))
        ));
    }

    #[test]
    fn rejects_node_floods() {
        let xml = wrap(&"<a/>".repeat(5000));
        assert!(Envelope::parse(xml.as_bytes(), &LIMITS).is_err());
    }

    #[test]
    fn rejects_non_utf8() {
        assert!(matches!(
            Envelope::parse(&[0xff, 0xfe, 0x00], &LIMITS),
            Err(SoapError::NotUtf8)
        ));
    }

    #[test]
    fn escape_covers_markup() {
        assert_eq!(escape("a&b<c>\"'"), "a&amp;b&lt;c&gt;&quot;&apos;");
        assert!(matches!(escape("plain"), Cow::Borrowed(_)));
    }

    #[test]
    fn fault_roundtrips_through_the_parser_shape() {
        let xml = Fault::action_not_supported().to_envelope();
        assert!(xml.contains("<s:Code><s:Value>s:Receiver</s:Value><s:Subcode><s:Value>ter:ActionNotSupported</s:Value></s:Subcode></s:Code>"));
        assert!(roxmltree::Document::parse(&xml).is_ok());
        assert_eq!(Fault::not_authorized().http_status(), 400);
        assert_eq!(Fault::internal().http_status(), 500);
    }

    proptest! {
        #[test]
        fn parse_never_panics_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = Envelope::parse(&bytes, &LIMITS);
        }

        #[test]
        fn parse_never_panics_on_xmlish_text(s in "[<>/a-zA-Z0-9 =\"'&;!?:\\[\\]-]{0,400}") {
            let _ = Envelope::parse(s.as_bytes(), &LIMITS);
        }

        #[test]
        fn excessive_depth_is_rejected(depth in 17usize..400) {
            let xml = wrap(&format!("{}{}", "<a>".repeat(depth), "</a>".repeat(depth)));
            prop_assert!(Envelope::parse(xml.as_bytes(), &LIMITS).is_err());
        }

        #[test]
        fn entity_declarations_are_rejected(name in "[a-z]{1,8}", value in "[a-z0-9 ]{0,20}", external in proptest::bool::ANY) {
            let decl = if external {
                format!(r#"<!ENTITY {name} SYSTEM "file:///{value}">"#)
            } else {
                format!(r#"<!ENTITY {name} "{value}">"#)
            };
            let xml = format!(
                r#"<?xml version="1.0"?><!DOCTYPE d [{decl}]><s:Envelope xmlns:s="{NS_SOAP12}"><s:Body><a>&{name};</a></s:Body></s:Envelope>"#
            );
            prop_assert!(Envelope::parse(xml.as_bytes(), &LIMITS).is_err());
        }

        #[test]
        fn oversized_inputs_are_rejected(extra in 1usize..4096) {
            let attr = "x".repeat(LIMITS.max_bytes + extra);
            let xml = wrap(&format!(r#"<a v="{attr}"/>"#));
            prop_assert!(matches!(Envelope::parse(xml.as_bytes(), &LIMITS), Err(SoapError::TooLarge(_))));
        }

        #[test]
        fn nested_bodies_never_panic(depth in 0usize..300, attr_len in 0usize..2000) {
            let open = format!(r#"<a v="{}">"#, "x".repeat(attr_len));
            let xml = wrap(&format!("{}t{}", open.repeat(depth), "</a>".repeat(depth)));
            let _ = Envelope::parse(xml.as_bytes(), &LIMITS);
        }
    }
}
