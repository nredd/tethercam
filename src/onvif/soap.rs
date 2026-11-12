//! SOAP 1.2 envelope parsing and response building.
//!
//! Parsing is total and bounded: input size, element depth and node count are capped, and
//! names are limited to 256 bytes and copied strings to four times the input size. Any DTD (so any entity declaration, internal or external) is rejected before it is
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
    /// The bounded XML preflight rejected the input.
    #[error("invalid or excessive XML before parsing: {0}")]
    Preflight(&'static str),
    /// The document has a DTD or entity declaration, or exceeds the node limit, or is not
    /// well-formed.
    #[error("malformed or forbidden XML: {0}")]
    Xml(#[from] roxmltree::Error),
    /// An XML namespace, element name, or attribute name exceeds 256 bytes.
    #[error("XML name or namespace exceeds 256 bytes")]
    NameTooLong,
    /// The converted tree would exceed its input-derived string budget.
    #[error("converted XML exceeds its allocation budget")]
    AllocationBudget,
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
#[derive(Clone, PartialEq, Eq)]
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

impl std::fmt::Debug for Element {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Element")
            .field("contents", &"<redacted>")
            .finish()
    }
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
        preflight(text, limits)?;
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

        validate_conversion(&doc, bytes.len())?;

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

/// Bounds checked before handing untrusted XML to the namespace-expanding parser.
const MAX_ATTRIBUTES: usize = 64;
const MAX_NAMESPACES: usize = 32;
const MAX_ATTRIBUTE_VALUE: usize = 4096;

/// Consume a bounded XML name, leaving full grammar validation to roxmltree.
fn preflight_name(input: &mut &str) -> Result<(), SoapError> {
    let remainder = input.trim_start_matches(|c: char| {
        !c.is_ascii_whitespace() && !matches!(c, '/' | '>' | '=' | '?' | '<' | '\'' | '"')
    });
    let length = input.len() - remainder.len();
    if length == 0 {
        return Err(SoapError::Preflight("missing name"));
    }
    if length > 256 {
        return Err(SoapError::NameTooLong);
    }
    *input = remainder;
    Ok(())
}

/// Consume one attribute and charge namespace declarations against the document limit.
fn preflight_attribute<'a>(
    mut input: &'a str, namespaces: &mut usize,
) -> Result<&'a str, SoapError> {
    let original = input;
    preflight_name(&mut input)?;
    let name_len = original.len() - input.len();
    let is_namespace =
        original.starts_with("xmlns:") || (name_len == 5 && original.starts_with("xmlns"));
    if is_namespace {
        *namespaces += 1;
        if *namespaces > MAX_NAMESPACES {
            return Err(SoapError::Preflight("too many namespaces"));
        }
    }
    input = input.trim_start_matches(|c: char| c.is_ascii_whitespace());
    input = input
        .strip_prefix('=')
        .ok_or(SoapError::Preflight("missing attribute equals"))?;
    input = input.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (quote, value) = if let Some(value) = input.strip_prefix('"') {
        ('"', value)
    } else if let Some(value) = input.strip_prefix('\'') {
        ('\'', value)
    } else {
        return Err(SoapError::Preflight("unquoted attribute"));
    };
    let (value, remainder) = value
        .split_once(quote)
        .ok_or(SoapError::Preflight("unfinished attribute"))?;
    if is_namespace && value.len() > 256 {
        return Err(SoapError::NameTooLong);
    }
    if value.len() > MAX_ATTRIBUTE_VALUE {
        return Err(SoapError::Preflight("attribute value too long"));
    }
    Ok(remainder)
}

/// Check lexical shape and resource bounds in one allocation-free pass.
fn preflight(mut input: &str, limits: &SoapLimits) -> Result<(), SoapError> {
    input = input.strip_prefix('\u{feff}').unwrap_or(input);
    if let Some(rest) = input.strip_prefix("<?xml") {
        if !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
            return Err(SoapError::Preflight("invalid XML declaration"));
        }
        let (declaration, remainder) = rest
            .split_once("?>")
            .ok_or(SoapError::Preflight("unfinished XML declaration"))?;
        if declaration.len() > MAX_ATTRIBUTE_VALUE {
            return Err(SoapError::Preflight("XML declaration too long"));
        }
        input = remainder;
    }
    let (mut depth, mut elements, mut namespaces) = (0usize, 0usize, 0usize);
    while !input.is_empty() {
        if let Some(rest) = input.strip_prefix('<') {
            input = rest;
        } else {
            input = input.trim_start_matches(|c| c != '<');
            continue;
        }
        if let Some(comment) = input.strip_prefix("!--") {
            let (_, remainder) = comment
                .split_once("-->")
                .ok_or(SoapError::Preflight("unfinished comment"))?;
            input = remainder;
            continue;
        }
        if input.starts_with(['!', '?']) {
            return Err(SoapError::Preflight("forbidden markup"));
        }
        if let Some(rest) = input.strip_prefix('/') {
            input = rest;
            preflight_name(&mut input)?;
            input = input.trim_start_matches(|c: char| c.is_ascii_whitespace());
            input = input
                .strip_prefix('>')
                .ok_or(SoapError::Preflight("invalid end tag"))?;
            depth = depth
                .checked_sub(1)
                .ok_or(SoapError::Preflight("unmatched end tag"))?;
            continue;
        }
        preflight_name(&mut input)?;
        elements += 1;
        if elements > limits.max_nodes as usize {
            return Err(SoapError::Preflight("too many elements"));
        }
        depth += 1;
        if depth > limits.max_depth {
            return Err(SoapError::TooDeep(limits.max_depth));
        }
        let mut attributes = 0usize;
        loop {
            let trimmed = input.trim_start_matches(|c: char| c.is_ascii_whitespace());
            let separated = trimmed.len() != input.len();
            input = trimmed;
            if let Some(rest) = input.strip_prefix("/>") {
                input = rest;
                depth -= 1;
                break;
            }
            if let Some(rest) = input.strip_prefix('>') {
                input = rest;
                break;
            }
            if !separated {
                return Err(SoapError::Preflight("attributes require whitespace"));
            }
            attributes += 1;
            if attributes > MAX_ATTRIBUTES {
                return Err(SoapError::Preflight("too many attributes"));
            }
            input = preflight_attribute(input, &mut namespaces)?;
        }
    }
    if depth != 0 {
        return Err(SoapError::Preflight("unclosed element"));
    }
    Ok(())
}

/// Check all names and copied string bytes before allocating the owned tree.
fn validate_conversion(doc: &Document<'_>, input_len: usize) -> Result<(), SoapError> {
    let mut bytes = 0usize;
    for node in doc.descendants().filter(Node::is_element) {
        let tag = node.tag_name();
        let namespace = tag.namespace().unwrap_or("");
        if namespace.len() > 256
            || tag.name().len() > 256
            || node.namespaces().any(|ns| ns.uri().len() > 256)
        {
            return Err(SoapError::NameTooLong);
        }
        bytes = bytes
            .saturating_add(namespace.len())
            .saturating_add(tag.name().len());
        for attribute in node.attributes() {
            if attribute.name().len() > 256 {
                return Err(SoapError::NameTooLong);
            }
            bytes = bytes
                .saturating_add(attribute.name().len())
                .saturating_add(attribute.value().len());
        }
        for text in node
            .children()
            .filter(Node::is_text)
            .filter_map(|n| n.text())
        {
            bytes = bytes.saturating_add(text.len());
        }
        if bytes > input_len.saturating_mul(4) {
            return Err(SoapError::AllocationBudget);
        }
    }
    Ok(())
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
    use std::fmt::Write as _;

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

    fn allocated_bytes(env: &Envelope) -> usize {
        fn size(e: &Element) -> usize {
            e.namespace.as_ref().map_or(0, String::capacity)
                + e.name.capacity()
                + e.text.capacity()
                + e.attributes.capacity() * std::mem::size_of::<(String, String)>()
                + e.attributes
                    .iter()
                    .map(|(n, v)| n.capacity() + v.capacity())
                    .sum::<usize>()
                + e.children.capacity() * std::mem::size_of::<Element>()
                + e.children.iter().map(size).sum::<usize>()
        }
        std::mem::size_of::<Envelope>()
            + env.header.capacity() * std::mem::size_of::<Element>()
            + size(&env.operation)
            + env.header.iter().map(size).sum::<usize>()
    }

    #[test]
    fn allocation_measurement_covers_headers_attributes_and_children() {
        let xml = format!(
            r#"<s:Envelope xmlns:s="{NS_SOAP12}"><s:Header><h a="v"><c/></h></s:Header><s:Body><op b="v"><child/></op></s:Body></s:Envelope>"#
        );
        let env = Envelope::parse(xml.as_bytes(), &LIMITS).unwrap();
        assert!(allocated_bytes(&env) <= xml.len() * 128);
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
            Err(SoapError::Preflight(_))
        ));
        let xxe = format!(
            r#"<?xml version="1.0"?><!DOCTYPE l [<!ENTITY x SYSTEM "file:///etc/passwd">]><s:Envelope xmlns:s="{NS_SOAP12}"><s:Body><x>&x;</x></s:Body></s:Envelope>"#
        );
        assert!(matches!(
            Envelope::parse(xxe.as_bytes(), &LIMITS),
            Err(SoapError::Preflight(_))
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
    fn preflight_rejects_nested_namespace_amplification() {
        let mut open = String::new();
        for i in 0..2_000 {
            write!(open, r#"<a xmlns:n{i}="u">"#).unwrap();
        }
        let xml = wrap(&format!("{open}{}", "</a>".repeat(2_000)));
        assert!(xml.len() < LIMITS.max_bytes);
        assert!(matches!(
            preflight(&xml, &LIMITS),
            Err(SoapError::TooDeep(16))
        ));
        assert!(matches!(
            Envelope::parse(xml.as_bytes(), &LIMITS),
            Err(SoapError::TooDeep(16))
        ));
        let deep = SoapLimits {
            max_depth: 3_000,
            ..LIMITS
        };
        assert!(matches!(
            preflight(&xml, &deep),
            Err(SoapError::Preflight("too many namespaces"))
        ));
        assert!(matches!(
            Envelope::parse(xml.as_bytes(), &deep),
            Err(SoapError::Preflight("too many namespaces"))
        ));
    }

    #[test]
    fn preflight_bounds_and_malformed_markup() {
        let mut attrs = String::new();
        for i in 0..65 {
            write!(attrs, r#" a{i}="x""#).unwrap();
        }
        let mut namespaces = String::new();
        for i in 0..33 {
            write!(namespaces, r#" xmlns:n{i}="u""#).unwrap();
        }
        let cases = [
            (format!("<a{attrs}/>"), "too many attributes"),
            (format!("<a{namespaces}/>"), "too many namespaces"),
            (
                format!(r#"<a x="{}"/>"#, "x".repeat(MAX_ATTRIBUTE_VALUE + 1)),
                "attribute value too long",
            ),
            ("<?xml-stylesheet?>".to_owned(), "invalid XML declaration"),
            (
                "<?xml version='1.0'".to_owned(),
                "unfinished XML declaration",
            ),
            (
                format!("<?xml {}?>", "x".repeat(MAX_ATTRIBUTE_VALUE + 1)),
                "XML declaration too long",
            ),
            ("<![CDATA[text]]>".to_owned(), "forbidden markup"),
            ("<!--unfinished".to_owned(), "unfinished comment"),
            ("<?other?>".to_owned(), "forbidden markup"),
            (
                "<a x='v'y='z'/>".to_owned(),
                "attributes require whitespace",
            ),
            ("<a x/>".to_owned(), "missing attribute equals"),
            ("<a x=v/>".to_owned(), "unquoted attribute"),
            ("<a x='v".to_owned(), "unfinished attribute"),
            ("<a></a x>".to_owned(), "invalid end tag"),
            ("</a>".to_owned(), "unmatched end tag"),
            ("<a>".to_owned(), "unclosed element"),
            ("<>".to_owned(), "missing name"),
        ];
        for (xml, expected) in cases {
            assert!(
                matches!(preflight(&xml, &LIMITS), Err(SoapError::Preflight(message)) if message == expected),
                "{xml}"
            );
        }
        let limited = SoapLimits {
            max_nodes: 1,
            ..LIMITS
        };
        assert!(matches!(
            preflight("<a><b/></a>", &limited),
            Err(SoapError::Preflight("too many elements"))
        ));
        assert!(preflight("\u{feff}<?xml\tversion='1.0'?><a />", &LIMITS).is_ok());
        let xml = wrap("<!-- ordinary comment --><a/>");
        assert!(Envelope::parse(xml.as_bytes(), &LIMITS).is_ok());
    }

    #[test]
    fn preflight_accepts_ws_security_envelope() {
        let xml = format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="{NS_SOAP12}" xmlns:tds="{NS_DEVICE}" xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd" xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd"><s:Header><wsse:Security><wsse:UsernameToken><wsse:Username>synthetic</wsse:Username><wsse:Password Type="digest">value</wsse:Password><wsu:Created>2025-01-01T00:00:00Z</wsu:Created></wsse:UsernameToken></wsse:Security></s:Header><s:Body><tds:GetHostname/></s:Body></s:Envelope>"#
        );
        assert!(preflight(&xml, &LIMITS).is_ok());
        assert!(Envelope::parse(xml.as_bytes(), &LIMITS).is_ok());
    }

    #[test]
    fn rejects_amplified_namespace() {
        let namespace = "x".repeat(45_000);
        let children = "<x:a/>".repeat(2_000);
        let xml = wrap(&format!(
            r#"<x:root xmlns:x="{namespace}">{children}</x:root>"#
        ));
        assert!(xml.len() < LIMITS.max_bytes);
        assert!(matches!(
            Envelope::parse(xml.as_bytes(), &LIMITS),
            Err(SoapError::NameTooLong)
        ));
    }

    #[test]
    fn conversion_checks_names_independently_of_preflight() {
        let name = "x".repeat(257);
        for xml in [
            format!("<{name}/>"),
            format!(r#"<a {name}="v"/>"#),
            format!(r#"<n:a xmlns:n="{name}"/>"#),
            format!(r#"<a xmlns:n="{name}"/>"#),
        ] {
            let doc = Document::parse(&xml).unwrap();
            assert!(matches!(
                validate_conversion(&doc, xml.len()),
                Err(SoapError::NameTooLong)
            ));
        }
    }

    #[test]
    fn rejects_names_and_conversion_budget() {
        let name = "x".repeat(257);
        for xml in [
            wrap(&format!("<{name}/>")),
            wrap(&format!(r#"<a {name}="v"/>"#)),
        ] {
            assert!(matches!(
                Envelope::parse(xml.as_bytes(), &LIMITS),
                Err(SoapError::NameTooLong)
            ));
        }
        let uri = "x".repeat(257);
        for xml in [
            wrap(&format!(r#"<a xmlns:x="{uri}" x:attr="v"/>"#)),
            wrap(&format!(r#"<a xmlns:x="{uri}"/>"#)),
        ] {
            assert!(matches!(
                Envelope::parse(xml.as_bytes(), &LIMITS),
                Err(SoapError::NameTooLong)
            ));
        }
        let xml = wrap(&format!(
            r#"<x:root xmlns:x="{}">{}</x:root>"#,
            "x".repeat(256),
            "<x:a/>".repeat(100)
        ));
        assert!(matches!(
            Envelope::parse(xml.as_bytes(), &LIMITS),
            Err(SoapError::AllocationBudget)
        ));
    }

    #[test]
    fn parsed_debug_redacts_security() {
        let xml = format!(
            r#"<s:Envelope xmlns:s="{NS_SOAP12}"><s:Header><Security><Password>synthetic-secret</Password></Security></s:Header><s:Body><a/></s:Body></s:Envelope>"#
        );
        let env = Envelope::parse(xml.as_bytes(), &LIMITS).unwrap();
        assert!(!format!("{env:?}").contains("synthetic-secret"));
        assert!(!format!("{:?}", env.header[0]).contains("synthetic-secret"));
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
        fn preflight_arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
            if let Ok(text) = std::str::from_utf8(&bytes) {
                let _ = preflight(text, &LIMITS);
            }
        }

        #[test]
        fn preflight_acceptance_respects_generated_limits(depth in 0usize..25, attrs in 0usize..75, namespaces in 0usize..40, value_len in 0usize..4200) {
            let mut declarations = String::new();
            for i in 0..namespaces { write!(declarations, r#" xmlns:n{i}="u""#).unwrap(); }
            let mut fields = String::new();
            for i in 0..attrs { write!(fields, r#" a{i}="{}""#, "x".repeat(value_len)).unwrap(); }
            let candidate = format!("{}<a{declarations}{fields}/>{}", "<a>".repeat(depth), "</a>".repeat(depth));
            if preflight(&candidate, &LIMITS).is_ok() {
                prop_assert!(depth < LIMITS.max_depth);
                prop_assert!(attrs + namespaces <= MAX_ATTRIBUTES);
                prop_assert!(namespaces <= MAX_NAMESPACES);
                prop_assert!(attrs == 0 || value_len <= MAX_ATTRIBUTE_VALUE);
            }
        }

        #[test]
        fn converted_strings_are_bounded(namespace in "[a-z]{1,256}", count in 0usize..300) {
            let xml = wrap(&format!(r#"<x:root xmlns:x="urn:{namespace}">{}</x:root>"#, "<x:a/>".repeat(count)));
            if let Ok(env) = Envelope::parse(xml.as_bytes(), &LIMITS) {
                prop_assert!(allocated_bytes(&env) <= xml.len() * 128);
            }
        }

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
