//! A small, total XML reader for WS-Discovery datagrams.
//!
//! Builds a bounded element tree with namespace resolution. It is deliberately strict where
//! strictness is a security property: DTDs and entity declarations are rejected (no entity
//! expansion), nesting depth and element count are capped, and only the five predefined
//! entities and numeric character references are decoded. Every input yields a tree or a
//! [`SyntaxError`]; nothing panics.
//!
//! References:
//! - [Extensible Markup Language 1.0](https://www.w3.org/TR/xml/)
//! - [Namespaces in XML 1.0](https://www.w3.org/TR/xml-names/)

use thiserror::Error;

/// Deepest element nesting accepted.
const MAX_DEPTH: usize = 16;
/// Most elements accepted in one document.
const MAX_ELEMENTS: usize = 256;
/// Most attributes accepted on one element.
const MAX_ATTRS: usize = 32;
/// The namespace the reserved `xml` prefix is bound to.
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// Why a document could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SyntaxError {
    /// The bytes are not valid UTF-8.
    #[error("input is not valid UTF-8")]
    NotUtf8,
    /// The document is not well-formed.
    #[error("malformed XML: {0}")]
    Malformed(&'static str),
    /// A DTD or entity declaration was present.
    #[error("DTDs and entity declarations are not allowed")]
    Doctype,
    /// Elements nest deeper than the limit.
    #[error("elements nest deeper than '{MAX_DEPTH}' levels")]
    TooDeep,
    /// The document has more elements than the limit.
    #[error("document has more than '{MAX_ELEMENTS}' elements")]
    TooManyElements,
    /// A name uses a namespace prefix that is not declared.
    #[error("namespace prefix is not declared")]
    UnboundPrefix,
}

/// A namespace-qualified name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QName {
    /// Namespace URI, if any.
    pub ns: Option<String>,
    /// Local part.
    pub local: String,
}

/// An unprefixed or prefixed attribute with its resolved namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Attr {
    pub(super) ns: Option<String>,
    pub(super) local: String,
    pub(super) value: String,
}

/// One element of the parsed tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Element {
    pub(super) ns: Option<String>,
    pub(super) local: String,
    pub(super) attrs: Vec<Attr>,
    pub(super) children: Vec<Element>,
    /// Concatenated direct text and CDATA, entities decoded.
    pub(super) text: String,
    /// In-scope namespace bindings (`""` is the default namespace).
    scope: Vec<(String, String)>,
}

impl Element {
    /// Whether this element has the given namespace and local name.
    pub(super) fn is(&self, ns: &str, local: &str) -> bool {
        self.ns.as_deref() == Some(ns) && self.local == local
    }

    /// First direct child with the given namespace and local name.
    pub(super) fn child(&self, ns: &str, local: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.is(ns, local))
    }

    /// Value of the first unprefixed attribute named `local`.
    pub(super) fn attr(&self, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.ns.is_none() && a.local == local)
            .map(|a| a.value.as_str())
    }

    /// Resolve a `prefix:local` (or bare `local`) token against this element's scope.
    pub(super) fn resolve_qname(&self, token: &str) -> Result<QName, SyntaxError> {
        match token.split_once(':') {
            Some((prefix, local)) => {
                let ns = lookup(&self.scope, prefix).ok_or(SyntaxError::UnboundPrefix)?;
                Ok(QName {
                    ns: Some(ns.to_owned()),
                    local: local.to_owned(),
                })
            }
            None => Ok(QName {
                ns: lookup(&self.scope, "")
                    .filter(|u| !u.is_empty())
                    .map(str::to_owned),
                local: token.to_owned(),
            }),
        }
    }
}

fn lookup<'s>(scope: &'s [(String, String)], prefix: &str) -> Option<&'s str> {
    if prefix == "xml" {
        return Some(XML_NS);
    }
    scope
        .iter()
        .rev()
        .find(|(p, _)| p == prefix)
        .map(|(_, u)| u.as_str())
}

/// Parse a document into its root element.
///
/// # Errors
///
/// Returns a [`SyntaxError`] for anything that is not a well-formed, in-budget document.
pub(super) fn parse(input: &str) -> Result<Element, SyntaxError> {
    let mut parser = Parser {
        rest: input.strip_prefix('\u{feff}').unwrap_or(input),
        elements: 0,
    };
    parser.skip_misc()?;
    let root = parser.element(0, &[])?;
    parser.skip_misc()?;
    if parser.rest.is_empty() {
        Ok(root)
    } else {
        Err(SyntaxError::Malformed("content after the root element"))
    }
}

struct Parser<'a> {
    rest: &'a str,
    elements: usize,
}

impl<'a> Parser<'a> {
    fn eat(&mut self, prefix: &str) -> bool {
        match self.rest.strip_prefix(prefix) {
            Some(rest) => {
                self.rest = rest;
                true
            }
            None => false,
        }
    }

    fn skip_ws(&mut self) {
        self.rest = self.rest.trim_start();
    }

    /// Consume up to and including `end`, returning the text before it.
    fn take_until(&mut self, end: &str) -> Result<&'a str, SyntaxError> {
        let unterminated = SyntaxError::Malformed("unterminated construct");
        let at = self.rest.find(end).ok_or(unterminated)?;
        let head = self.rest.get(..at).ok_or(unterminated)?;
        self.rest = self.rest.get(at + end.len()..).ok_or(unterminated)?;
        Ok(head)
    }

    /// Consume a name: everything up to whitespace, `/`, `>` or `=`.
    fn take_name(&mut self) -> Result<&'a str, SyntaxError> {
        let end = self
            .rest
            .find(|c: char| c.is_whitespace() || matches!(c, '/' | '>' | '='))
            .unwrap_or(self.rest.len());
        let bad = SyntaxError::Malformed("expected a name");
        let name = self.rest.get(..end).filter(|n| !n.is_empty()).ok_or(bad)?;
        self.rest = self.rest.get(end..).ok_or(bad)?;
        Ok(name)
    }

    /// Skip whitespace, processing instructions and comments; reject DTDs.
    fn skip_misc(&mut self) -> Result<(), SyntaxError> {
        loop {
            self.skip_ws();
            if self.eat("<?") {
                self.take_until("?>")?;
            } else if self.eat("<!--") {
                self.take_until("-->")?;
            } else if self.rest.starts_with("<!") {
                return Err(SyntaxError::Doctype);
            } else {
                return Ok(());
            }
        }
    }

    fn element(
        &mut self, depth: usize, parent_scope: &[(String, String)],
    ) -> Result<Element, SyntaxError> {
        if depth >= MAX_DEPTH {
            return Err(SyntaxError::TooDeep);
        }
        self.elements += 1;
        if self.elements > MAX_ELEMENTS {
            return Err(SyntaxError::TooManyElements);
        }
        if !self.eat("<") {
            return Err(SyntaxError::Malformed("expected an element"));
        }
        let raw_name = self.take_name()?;
        let (raw_attrs, empty) = self.attributes()?;

        let mut scope = parent_scope.to_vec();
        for (name, value) in &raw_attrs {
            if name == "xmlns" {
                scope.push((String::new(), value.clone()));
            } else if let Some(prefix) = name.strip_prefix("xmlns:") {
                scope.push((prefix.to_owned(), value.clone()));
            }
        }

        let (ns, local) = resolve_element(&scope, raw_name)?;
        let mut attrs = Vec::new();
        for (name, value) in raw_attrs {
            if name == "xmlns" || name.starts_with("xmlns:") {
                continue;
            }
            let (ns, local) = match name.split_once(':') {
                Some((prefix, local)) => (
                    Some(
                        lookup(&scope, prefix)
                            .ok_or(SyntaxError::UnboundPrefix)?
                            .to_owned(),
                    ),
                    local.to_owned(),
                ),
                None => (None, name),
            };
            attrs.push(Attr { ns, local, value });
        }

        let mut children = Vec::new();
        let mut text = String::new();
        if !empty {
            self.content(raw_name, depth, &scope, &mut children, &mut text)?;
        }
        Ok(Element {
            ns,
            local,
            attrs,
            children,
            text,
            scope,
        })
    }

    /// Parse attributes up to the end of the start tag. Returns them and whether it was `/>`.
    fn attributes(&mut self) -> Result<(Vec<(String, String)>, bool), SyntaxError> {
        let mut attrs = Vec::new();
        loop {
            self.skip_ws();
            if self.eat("/>") {
                return Ok((attrs, true));
            }
            if self.eat(">") {
                return Ok((attrs, false));
            }
            if attrs.len() >= MAX_ATTRS {
                return Err(SyntaxError::Malformed("too many attributes"));
            }
            let name = self.take_name()?;
            self.skip_ws();
            if !self.eat("=") {
                return Err(SyntaxError::Malformed("attribute without a value"));
            }
            self.skip_ws();
            let quote = if self.eat("\"") {
                "\""
            } else if self.eat("'") {
                "'"
            } else {
                return Err(SyntaxError::Malformed("unquoted attribute value"));
            };
            let value = decode(self.take_until(quote)?)?;
            attrs.push((name.to_owned(), value));
        }
    }

    fn content(
        &mut self, raw_name: &str, depth: usize, scope: &[(String, String)],
        children: &mut Vec<Element>, text: &mut String,
    ) -> Result<(), SyntaxError> {
        loop {
            let unterminated = SyntaxError::Malformed("unterminated element");
            let at = self.rest.find('<').ok_or(unterminated)?;
            let chunk = self.rest.get(..at).ok_or(unterminated)?;
            text.push_str(&decode(chunk)?);
            self.rest = self.rest.get(at..).ok_or(unterminated)?;
            if self.eat("</") {
                if self.take_until(">")?.trim() != raw_name {
                    return Err(SyntaxError::Malformed("mismatched end tag"));
                }
                return Ok(());
            } else if self.eat("<!--") {
                self.take_until("-->")?;
            } else if self.eat("<![CDATA[") {
                let cdata = self.take_until("]]>")?;
                check_chars(cdata)?;
                text.push_str(cdata);
            } else if self.eat("<?") {
                self.take_until("?>")?;
            } else if self.rest.starts_with("<!") {
                return Err(SyntaxError::Doctype);
            } else {
                children.push(self.element(depth + 1, scope)?);
            }
        }
    }
}

fn resolve_element(
    scope: &[(String, String)], raw: &str,
) -> Result<(Option<String>, String), SyntaxError> {
    match raw.split_once(':') {
        Some((prefix, local)) => {
            let ns = lookup(scope, prefix).ok_or(SyntaxError::UnboundPrefix)?;
            Ok((Some(ns.to_owned()), local.to_owned()))
        }
        None => Ok((
            lookup(scope, "")
                .filter(|u| !u.is_empty())
                .map(str::to_owned),
            raw.to_owned(),
        )),
    }
}

/// Whether `c` is allowed in an XML 1.0 document (`Char` production).
fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..)
}

fn check_chars(s: &str) -> Result<(), SyntaxError> {
    if s.chars().all(is_xml_char) {
        Ok(())
    } else {
        Err(SyntaxError::Malformed("character not allowed in XML"))
    }
}

/// Decode the predefined entities and numeric character references.
fn decode(input: &str) -> Result<String, SyntaxError> {
    check_chars(input)?;
    let bad = SyntaxError::Malformed("invalid entity reference");
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find('&') {
        out.push_str(rest.get(..at).ok_or(bad)?);
        let after = rest.get(at + 1..).ok_or(bad)?;
        let semi = after.find(';').filter(|&i| i <= 10).ok_or(bad)?;
        let name = after.get(..semi).ok_or(bad)?;
        out.push(match name {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let digits = name.strip_prefix('#').ok_or(bad)?;
                let code = match digits.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16),
                    None => digits.parse(),
                }
                .map_err(|_| bad)?;
                char::from_u32(code)
                    .filter(|c| is_xml_char(*c))
                    .ok_or(bad)?
            }
        });
        rest = after.get(semi + 1..).ok_or(bad)?;
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parses_namespaces_attrs_and_text() {
        let doc = r#"<?xml version="1.0"?><!-- c --><a:r xmlns:a="urn:a" xmlns="urn:d" k='v&amp;w'>
            <x>1 &lt; 2 &#65;&#x42;<![CDATA[<raw>]]></x><a:y/></a:r>"#;
        let root = parse(doc).unwrap();
        assert!(root.is("urn:a", "r"));
        assert_eq!(root.attr("k"), Some("v&w"));
        let x = root.child("urn:d", "x").unwrap();
        assert_eq!(x.text, "1 < 2 AB<raw>");
        assert!(root.child("urn:a", "y").is_some());
    }

    #[test]
    fn resolves_qnames_with_shadowing() {
        let root = parse(r#"<r xmlns:p="urn:1"><c xmlns:p="urn:2"/></r>"#).unwrap();
        let q = root.children[0].resolve_qname("p:t").unwrap();
        assert_eq!(q.ns.as_deref(), Some("urn:2"));
        assert_eq!(
            root.resolve_qname("q:t").unwrap_err(),
            SyntaxError::UnboundPrefix
        );
        assert_eq!(root.resolve_qname("t").unwrap().ns, None);
    }

    #[test]
    fn rejects_hostile_and_malformed_input() {
        let lol = r#"<!DOCTYPE l [<!ENTITY a "aaaa">]><r>&a;</r>"#;
        assert_eq!(parse(lol).unwrap_err(), SyntaxError::Doctype);
        for bad in [
            "",
            "<a>",
            "<a></b>",
            "<a/><b/>",
            "<a k=v/>",
            "<a>&bogus;</a>",
            "<a>&#xD800;</a>",
            "<a>&#0;</a>",
            "<a>&#1;</a>",
            "<a>\u{1}</a>",
            "<a><![CDATA[\u{2}]]></a>",
            "<p:a/>",
            "<a",
            "<a k=\"v",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn enforces_depth_and_element_budgets() {
        let deep = format!("{}{}", "<a>".repeat(40), "</a>".repeat(40));
        assert_eq!(parse(&deep).unwrap_err(), SyntaxError::TooDeep);
        let wide = format!("<r>{}</r>", "<a/>".repeat(300));
        assert_eq!(parse(&wide).unwrap_err(), SyntaxError::TooManyElements);
    }

    proptest! {
        #[test]
        fn never_panics_on_arbitrary_text(s in ".{0,512}") {
            let _ = parse(&s);
        }

        #[test]
        fn never_panics_on_xmlish_text(s in "[<>/a-z:=\"'& ;#!\\[\\]?x0-9-]{0,256}") {
            let _ = parse(&s);
        }
    }
}
