use super::schema::{self, Definition, Domain, Field, Kind, Metadata, Types};
use heck::{ToSnakeCase, ToUpperCamelCase};
use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;

fn ident(name: &str) -> String {
    let name = name.to_snake_case();
    match name.as_str() {
        "self" | "super" | "crate" | "type" | "ref" | "move" | "match" | "loop" | "in" | "for"
        | "mod" | "use" | "fn" | "struct" | "enum" | "const" | "static" | "async" | "await"
        | "dyn" | "impl" | "trait" | "where" | "return" | "break" | "continue" | "if" | "else"
        | "while" | "let" | "pub" | "unsafe" | "extern" | "override" | "abstract" | "become"
        | "final" | "macro" | "priv" | "typeof" | "unsized" | "virtual" | "do" | "as" | "true"
        | "false" | "box" | "try" | "yield" | "gen" => format!("{name}_"),
        _ => name,
    }
}

fn type_name(name: &str) -> String {
    let name = name.to_upper_camel_case();
    if name == "Self" {
        "SelfValue".into()
    } else {
        name
    }
}

fn optional(owner: &str, field: &Field) -> bool {
    // Chrome 151 omits this newer WebAudio event field.
    field.optional || (owner == "WebAudio.BaseAudioContext" && field.name == "renderQuantumSize")
}

fn prose(mut text: &str) -> String {
    fn escape(text: &str) -> String {
        text.replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('[', "\\[")
            .replace(']', "\\]")
    }
    let mut out = String::new();
    while let Some(start) = [text.find("https://"), text.find("http://")]
        .into_iter()
        .flatten()
        .min()
    {
        out.push_str(&escape(&text[..start]));
        text = &text[start..];
        let end = text
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '\"' | ')' | ']'))
            .unwrap_or(text.len());
        let url = text[..end].trim_end_matches(['.', ',', ';']);
        out.push('<');
        out.push_str(url);
        out.push('>');
        text = &text[url.len()..];
    }
    out.push_str(&escape(text));
    out
}

// Protocol prose is not Rust: preserve code spans while escaping HTML and
// bracket ranges outside them so rustdoc does not interpret them as links/tags.
fn documentation(value: &Metadata) -> TokenStream {
    let mut out = TokenStream::new();
    if let Some(description) = value.description.as_deref() {
        let text = description
            .split('`')
            .enumerate()
            .map(|(index, part)| {
                if index % 2 == 0 {
                    prose(part)
                } else {
                    part.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("`");
        out.extend(quote!(#[doc = #text]));
    }
    if value.experimental {
        out.extend(quote!(#[doc = "\nExperimental in the pinned CDP schema."]));
    }
    if value.deprecated {
        out.extend(quote!(#[doc = "\nDeprecated in the pinned CDP schema."]));
    }
    out
}

fn symbol(name: &str) -> Ident {
    Ident::new(name, Span::call_site())
}

pub(super) struct Generator<'a> {
    types: Types<'a>,
}

impl Generator<'_> {
    fn rust_type(
        &self,
        domain: &str,
        owner: &str,
        name: &str,
        value: &Definition,
        extra: &mut TokenStream,
    ) -> TokenStream {
        match &value.kind {
            Kind::Reference(reference) => {
                let key = schema::reference(domain, reference);
                let (target_domain, target_name) =
                    key.split_once('.').expect("qualified reference");
                let module = symbol(&ident(target_domain));
                let target = symbol(&type_name(target_name));
                let path = quote!(crate::cdp::#module::#target);
                if self.types.needs_box(&key, owner) {
                    quote!(Box<#path>)
                } else {
                    path
                }
            }
            Kind::String(Some(values)) => {
                extra.extend(self.enumeration(name, &value.metadata, values));
                let name = symbol(name);
                quote!(#name)
            }
            Kind::String(None) => quote!(String),
            Kind::Integer => quote!(i64),
            Kind::Number => quote!(f64),
            Kind::Boolean => quote!(bool),
            Kind::Any => quote!(serde_json::Value),
            Kind::Object(Some(fields)) => {
                extra.extend(documentation(&value.metadata));
                extra.extend(self.structure(domain, owner, name, fields));
                let name = symbol(name);
                quote!(#name)
            }
            Kind::Object(None) => quote!(serde_json::Map<String, serde_json::Value>),
            Kind::Array(items) => {
                let inner = self.rust_type(domain, "", &format!("{name}Item"), items, extra);
                quote!(Vec<#inner>)
            }
        }
    }

    fn enumeration(&self, name: &str, metadata: &Metadata, values: &[String]) -> TokenStream {
        let docs = documentation(metadata);
        let mut used = std::collections::HashSet::from(["Unrecognized".to_owned()]);
        let variants: Vec<_> = values
            .iter()
            .map(|wire| {
                let mut variant = type_name(wire);
                if variant.is_empty() {
                    variant = "Empty".into();
                }
                if variant.starts_with(|c: char| c.is_ascii_digit()) {
                    variant.insert_str(0, "Value");
                }
                assert!(
                    used.insert(variant.clone()),
                    "enum name collision: {name}.{wire}"
                );
                symbol(&variant)
            })
            .collect();
        let name = symbol(name);
        quote! {
            #docs
            #[derive(Debug, Clone, PartialEq, Eq, Hash)]
            #[non_exhaustive]
            pub enum #name {
                #(#variants,)*
                /// A wire string absent from the pinned schema, preserved verbatim.
                Unrecognized(String),
            }
            impl #name {
                /// Return the exact protocol wire string, including unrecognized values.
                pub fn as_str(&self) -> &str {
                    match self {
                        #(Self::#variants => #values,)*
                        Self::Unrecognized(value) => value.as_str(),
                    }
                }
            }
            impl serde::Serialize for #name {
                fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
                    serializer.serialize_str(self.as_str())
                }
            }
            impl<'de> serde::Deserialize<'de> for #name {
                fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
                    let value = <String as serde::Deserialize<'de>>::deserialize(deserializer)?;
                    Ok(match value.as_str() {
                        #(#values => Self::#variants,)*
                        _ => Self::Unrecognized(value),
                    })
                }
            }
        }
    }

    fn structure(&self, domain: &str, owner: &str, name: &str, fields: &[Field]) -> TokenStream {
        let mut extra = TokenStream::new();
        let default = fields
            .iter()
            .all(|f| optional(owner, f))
            .then(|| quote!(, Default));
        let members: Vec<_> = fields
            .iter()
            .map(|field| {
                let wire = field.name.as_str();
                let member = symbol(&ident(wire));
                let mut ty = self.rust_type(
                    domain,
                    owner,
                    &format!("{name}{}", type_name(wire)),
                    &field.definition,
                    &mut extra,
                );
                let docs = documentation(&field.definition.metadata);
                let compatibility = (optional(owner, field) && !field.optional).then(|| {
                    quote! {
                        /// Absent on Chrome 151; optional for compatibility.
                    }
                });
                let mut attributes = quote!(#[serde(rename = #wire)]);
                if optional(owner, field) {
                    ty = quote!(Option<#ty>);
                    attributes
                        .extend(quote!(#[serde(default, skip_serializing_if = "Option::is_none")]));
                    if self.types.is_any(domain, &field.definition) {
                        attributes.extend(
                            quote!(#[serde(deserialize_with = "crate::protocol::present_json")]),
                        );
                    }
                }
                quote!(#docs #compatibility #attributes pub #member: #ty,)
            })
            .collect();
        let name = symbol(name);
        quote! {
            #[derive(Debug, Clone, serde::Serialize, serde::Deserialize #default)]
            pub struct #name { #(#members)* }
            #extra
        }
    }

    fn constructor(&self, domain: &str, owner: &str, name: &str, fields: &[Field]) -> TokenStream {
        let types: Vec<_> = fields
            .iter()
            .map(|field| {
                let ty = self.rust_type(
                    domain,
                    owner,
                    &format!("{name}{}", type_name(&field.name)),
                    &field.definition,
                    &mut TokenStream::new(),
                );
                (field, symbol(&ident(&field.name)), ty)
            })
            .collect();
        let args: Vec<_> = types
            .iter()
            .filter(|(field, _, _)| !optional(owner, field))
            .map(|(_, member, ty)| quote!(#member: impl Into<#ty>))
            .collect();
        let allowance = (args.len() > 7).then(|| quote!(#[allow(clippy::too_many_arguments)]));
        let members = types.iter().map(|(field, member, _)| {
            let value = if optional(owner, field) {
                quote!(None)
            } else {
                quote!(#member.into())
            };
            quote!(#member: #value,)
        });
        let setters = types
            .iter()
            .filter(|(field, _, _)| optional(owner, field))
            .map(|(field, member, ty)| {
                let docs = documentation(&field.definition.metadata);
                let method = symbol(&format!("with_{member}"));
                quote! {
                    #docs
                    #[must_use]
                    pub fn #method(mut self, value: impl Into<#ty>) -> Self {
                        self.#member = Some(value.into());
                        self
                    }
                }
            });
        let name = symbol(name);
        quote! {
            impl #name {
                /// Construct a value with its required fields. Optional fields are omitted.
                #[must_use]
                #allowance
                pub fn new(#(#args),*) -> Self { Self { #(#members)* } }
                #(#setters)*
            }
        }
    }

    fn newtype(&self, name: &str, value: &Definition, ty: &TokenStream) -> TokenStream {
        let name = symbol(name);
        let docs = documentation(&value.metadata);
        let eq = matches!(value.kind, Kind::String(None) | Kind::Integer)
            .then(|| quote!(, PartialEq, Eq, Hash));
        let scalar = matches!(
            value.kind,
            Kind::String(None) | Kind::Integer | Kind::Number | Kind::Boolean
        )
        .then(|| {
            quote! {
                impl From<#ty> for #name {
                    fn from(value: #ty) -> Self { Self(value) }
                }
                impl std::fmt::Display for #name {
                    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        std::fmt::Display::fmt(&self.0, f)
                    }
                }
            }
        });
        let string = matches!(value.kind, Kind::String(None)).then(|| {
            quote! {
                impl From<&str> for #name {
                    fn from(value: &str) -> Self { Self(value.to_owned()) }
                }
                impl AsRef<str> for #name {
                    fn as_ref(&self) -> &str { &self.0 }
                }
            }
        });
        quote! {
            #docs
            #[derive(Debug, Clone, serde::Serialize, serde::Deserialize #eq)]
            #[serde(transparent)]
            pub struct #name(pub #ty);
            #scalar
            #string
        }
    }

    fn domain(&self, domain: &Domain) -> TokenStream {
        let domain_name = domain.domain.as_str();
        let docs = documentation(&domain.metadata);
        let module = symbol(&ident(domain_name));
        let types = domain.types.iter().map(|named| {
            let value = &named.definition;
            let name = type_name(&named.id);
            let owner = schema::reference(domain_name, &named.id);
            let mut output = TokenStream::new();
            let ty = self.rust_type(domain_name, &owner, &name, value, &mut output);
            match &value.kind {
                Kind::Object(Some(fields)) => {
                    output.extend(self.constructor(domain_name, &owner, &name, fields))
                }
                Kind::String(Some(_)) => {}
                _ => output.extend(self.newtype(&name, value, &ty)),
            }
            output
        });
        let commands = domain.commands.iter().map(|command| {
            let name = type_name(&command.name);
            let response_name = format!("{name}Response");
            let docs = documentation(&command.metadata);
            let request = self.structure(domain_name, "", &name, &command.parameters);
            let constructor = self.constructor(domain_name, "", &name, &command.parameters);
            let response = self.structure(domain_name, "", &response_name, &command.returns);
            let name = symbol(&name);
            let response_name = symbol(&response_name);
            let method = format!("{domain_name}.{}", command.name);
            quote! {
                #docs #request #constructor #response
                impl crate::protocol::Command for #name {
                    type Response = #response_name;
                    const METHOD: &'static str = #method;
                }
            }
        });
        let events = domain.events.iter().map(|event| {
            let name = type_name(&event.name);
            let docs = documentation(&event.metadata);
            let payload = self.structure(domain_name, "", &name, &event.parameters);
            let name = symbol(&name);
            let method = format!("{domain_name}.{}", event.name);
            quote! {
                #docs #payload
                impl crate::protocol::Event for #name {
                    const METHOD: &'static str = #method;
                }
            }
        });
        quote! {
            #docs
            pub mod #module {
                #(#types)*
                pub mod commands { #(#commands)* }
                pub mod events { #(#events)* }
            }
        }
    }
}

pub(super) fn generate(domains: &[Domain]) -> TokenStream {
    let generator = Generator {
        types: Types::new(domains),
    };
    domains
        .iter()
        .map(|domain| generator.domain(domain))
        .collect()
}
