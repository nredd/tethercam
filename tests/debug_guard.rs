//! Guard against deriving `Debug` directly on raw credential fields and header maps.

use std::fs;
use std::path::Path;

use syn::visit::Visit;

#[derive(Default)]
struct Guard {
    violations: Vec<String>,
}

fn derives_debug(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .filter(|a| a.path().is_ident("derive"))
        .any(|a| {
            a.parse_args_with(
                syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
            )
            .is_ok_and(|paths| {
                paths
                    .iter()
                    .any(|p| p.segments.last().is_some_and(|s| s.ident == "Debug"))
            })
        })
}

fn sensitive_fields(type_name: &str, fields: &syn::Fields) -> bool {
    fields.iter().any(|field| {
        let Some(name) = &field.ident else {
            return false;
        };
        let name = name.to_string();
        let request_body = name == "body" && type_name.contains("Request");
        if !request_body
            && ![
                "password",
                "authorization",
                "response",
                "headers",
                "nonce",
                "nonces",
                "www_authenticate",
            ]
            .contains(&name.as_str())
        {
            return false;
        }
        match &field.ty {
            syn::Type::Path(path) => {
                !path.path.segments.last().is_some_and(|s| {
                    s.ident == "Secret" || s.ident == "Headers" || s.ident == "u16"
                })
            }
            _ => true,
        }
    })
}

impl<'ast> Visit<'ast> for Guard {
    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        if derives_debug(&item.attrs)
            && (item.ident == "Headers" || sensitive_fields(&item.ident.to_string(), &item.fields))
        {
            self.violations.push(item.ident.to_string());
        }
        syn::visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        if derives_debug(&item.attrs)
            && item.variants.iter().any(|variant| {
                sensitive_fields(&format!("{}{}", item.ident, variant.ident), &variant.fields)
            })
        {
            self.violations.push(item.ident.to_string());
        }
        syn::visit::visit_item_enum(self, item);
    }
}

fn scan_dir(path: &Path, guard: &mut Guard) -> Result<(), Box<dyn std::error::Error>> {
    for entry in fs::read_dir(path)? {
        let path = entry?.path();
        if path.is_dir() {
            scan_dir(&path, guard)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            let source = fs::read_to_string(&path)?;
            let file = syn::parse_file(&source)?;
            guard.visit_file(&file);
        }
    }
    Ok(())
}

#[test]
fn no_derived_debug_on_raw_secrets() -> Result<(), Box<dyn std::error::Error>> {
    let mut guard = Guard::default();
    scan_dir(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src").as_path(),
        &mut guard,
    )?;
    assert!(guard.violations.is_empty(), "{:?}", guard.violations);
    Ok(())
}

#[test]
fn guard_detects_raw_secrets_and_header_maps() {
    let file = syn::parse_file(
        r"
        #[derive(Debug)] struct Bad { password: String }
        #[derive(Debug)] struct AlsoBad { authorization: Option<String> }
        #[derive(Debug)] struct Headers(Vec<(String, String)>);
        #[derive(Debug)] struct Request { body: Vec<u8> }
        #[derive(Debug)] struct Challenge { nonce: String, www_authenticate: String }
        #[derive(Debug)] struct Safe { password: Secret, headers: Headers }
        #[derive(std::fmt::Debug)] struct Qualified { password: String }
        #[derive(Debug)] enum BadEnum { Variant { password: String } }
        #[derive(Debug)] enum Messages { Request { body: Vec<u8> } }
    ",
    )
    .unwrap();
    let mut guard = Guard::default();
    guard.visit_file(&file);
    assert_eq!(
        guard.violations,
        [
            "Bad",
            "AlsoBad",
            "Headers",
            "Request",
            "Challenge",
            "Qualified",
            "BadEnum",
            "Messages"
        ]
    );
}
