use super::schema::{self, Definition, Domain, Field, Kind, Metadata, Types};
use heck::{ToSnakeCase, ToUpperCamelCase};

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
fn documentation(value: &Metadata, out: &mut String) {
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
        out.push_str(&format!("#[doc = {text:?}]\n"));
    }
    if value.experimental {
        out.push_str("#[doc = \"\nExperimental in the pinned CDP schema.\"]\n");
    }
    if value.deprecated {
        out.push_str("#[doc = \"\nDeprecated in the pinned CDP schema.\"]\n");
    }
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
        extra: &mut String,
    ) -> String {
        if let Kind::Reference(reference) = &value.kind {
            let key = schema::reference(domain, reference);
            let (target_domain, target_name) = key.split_once('.').expect("qualified reference");
            let path = format!(
                "crate::cdp::{}::{}",
                ident(target_domain),
                type_name(target_name)
            );
            return if self.types.needs_box(&key, owner) {
                format!("Box<{path}>")
            } else {
                path
            };
        }
        if let Kind::String(Some(values)) = &value.kind {
            documentation(&value.metadata, extra);
            extra.push_str(&format!("#[derive(Debug, Clone, PartialEq, Eq, Hash)]\n#[non_exhaustive]\npub enum {name} {{\n"));
            let mut used = std::collections::HashSet::new();
            // Some schemas already define a known `Unknown` variant. Reserve
            // one uniform fallback name without renaming any known variants.
            used.insert("Unrecognized".to_owned());
            let mut variants = Vec::new();
            for variant in values {
                let wire = variant.as_str();
                let mut variant_name = type_name(wire);
                if variant_name.is_empty() {
                    variant_name = "Empty".into();
                }
                if variant_name.starts_with(|c: char| c.is_ascii_digit()) {
                    variant_name.insert_str(0, "Value");
                }
                assert!(
                    used.insert(variant_name.clone()),
                    "enum name collision: {name}.{wire}"
                );
                extra.push_str(&format!("{variant_name},\n"));
                variants.push((variant_name, wire));
            }
            extra.push_str("/// A wire string absent from the pinned schema, preserved verbatim.\nUnrecognized(String),\n}\n");
            extra.push_str(&format!("impl {name} {{\n/// Return the exact protocol wire string, including unrecognized values.\npub fn as_str(&self) -> &str {{ match self {{\n"));
            for (variant, wire) in &variants {
                extra.push_str(&format!("Self::{variant} => {wire:?},\n"));
            }
            extra.push_str("Self::Unrecognized(value) => value.as_str(),\n} }\n}\n");
            extra.push_str(&format!("impl serde::Serialize for {name} {{\nfn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {{ serializer.serialize_str(self.as_str()) }}\n}}\n"));
            extra.push_str(&format!("impl<'de> serde::Deserialize<'de> for {name} {{\nfn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {{\nlet value = <String as serde::Deserialize<'de>>::deserialize(deserializer)?;\nOk(match value.as_str() {{\n"));
            for (variant, wire) in &variants {
                extra.push_str(&format!("{wire:?} => Self::{variant},\n"));
            }
            extra.push_str("_ => Self::Unrecognized(value),\n})\n}\n}\n");
            return name.into();
        }
        match &value.kind {
            Kind::String(_) => "String".into(),
            Kind::Integer => "i64".into(),
            Kind::Number => "f64".into(),
            Kind::Boolean => "bool".into(),
            Kind::Any => "serde_json::Value".into(),
            Kind::Object(Some(fields)) => {
                documentation(&value.metadata, extra);
                self.structure(domain, owner, name, fields, extra);
                name.into()
            }
            Kind::Object(None) => "serde_json::Map<String, serde_json::Value>".into(),
            Kind::Array(items) => {
                let inner = self.rust_type(domain, "", &format!("{name}Item"), items, extra);
                format!("Vec<{inner}>")
            }
            Kind::Reference(_) => unreachable!("references handled above"),
        }
    }

    fn structure(&self, domain: &str, owner: &str, name: &str, fields: &[Field], out: &mut String) {
        let mut extra = String::new();
        let default = if fields.iter().all(|f| optional(owner, f)) {
            ", Default"
        } else {
            ""
        };
        out.push_str(&format!("#[derive(Debug, Clone, serde::Serialize, serde::Deserialize{default})]\npub struct {name} {{\n"));
        for field in fields {
            let wire = field.name.as_str();
            let mut ty = self.rust_type(
                domain,
                owner,
                &format!("{name}{}", type_name(wire)),
                &field.definition,
                &mut extra,
            );
            documentation(&field.definition.metadata, out);
            if optional(owner, field) && !field.optional {
                out.push_str("/// Absent on Chrome 151; optional for compatibility.\n");
            }
            out.push_str(&format!("#[serde(rename = {wire:?})]\n"));
            if optional(owner, field) {
                ty = format!("Option<{ty}>");
                out.push_str("#[serde(default, skip_serializing_if = \"Option::is_none\")]\n");
                if self.types.is_any(domain, &field.definition) {
                    out.push_str(
                        "#[serde(deserialize_with = \"crate::protocol::present_json\")]\n",
                    );
                }
            }
            out.push_str(&format!("pub {}: {ty},\n", ident(wire)));
        }
        out.push_str("}\n");
        out.push_str(&extra);
    }

    fn constructor(
        &self,
        domain: &str,
        owner: &str,
        name: &str,
        fields: &[Field],
        out: &mut String,
    ) {
        let types: Vec<_> = fields
            .iter()
            .map(|field| {
                let wire = field.name.as_str();
                let ty = self.rust_type(
                    domain,
                    owner,
                    &format!("{name}{}", type_name(wire)),
                    &field.definition,
                    &mut String::new(),
                );
                (field, ident(wire), ty)
            })
            .collect();
        let required: Vec<_> = types
            .iter()
            .filter(|(field, _, _)| !optional(owner, field))
            .collect();
        out.push_str(&format!("impl {name} {{\n"));
        out.push_str("/// Construct a value with its required fields. Optional fields are omitted.\n#[must_use]\n");
        if required.len() > 7 {
            out.push_str("#[allow(clippy::too_many_arguments)]\n");
        }
        let args = required
            .iter()
            .map(|(_, field, ty)| format!("{field}: impl Into<{ty}>"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("pub fn new({args}) -> Self {{ Self {{\n"));
        for (field, member, _) in &types {
            let value = if optional(owner, field) {
                "None".into()
            } else {
                format!("{member}.into()")
            };
            out.push_str(&format!("{member}: {value},\n"));
        }
        out.push_str("} }\n");
        for (field, member, ty) in &types {
            if optional(owner, field) {
                documentation(&field.definition.metadata, out);
                out.push_str(&format!("#[must_use]\npub fn with_{member}(mut self, value: impl Into<{ty}>) -> Self {{ self.{member} = Some(value.into()); self }}\n"));
            }
        }
        out.push_str("}\n");
    }

    fn domain(&self, domain: &Domain, out: &mut String) {
        let domain_name = domain.domain.as_str();
        documentation(&domain.metadata, out);
        out.push_str(&format!("pub mod {} {{\n", ident(domain_name)));
        for named in &domain.types {
            let id = named.id.as_str();
            let value = &named.definition;
            let name = type_name(id);
            let owner = format!("{domain_name}.{id}");
            let ty = self.rust_type(domain_name, &owner, &name, value, out);
            if let Kind::Object(Some(fields)) = &value.kind {
                self.constructor(domain_name, &owner, &name, fields, out);
            }
            if ty != name {
                documentation(&value.metadata, out);
                // Transparent newtypes keep protocol identifiers distinct.
                let eq = if matches!(value.kind, Kind::String(_) | Kind::Integer) {
                    ", PartialEq, Eq, Hash"
                } else {
                    ""
                };
                out.push_str(&format!("#[derive(Debug, Clone, serde::Serialize, serde::Deserialize{eq})]\n#[serde(transparent)]\npub struct {name}(pub {ty});\n"));
                // Only scalar wrappers get these conversions. Enums and named
                // objects were emitted above; arrays and maps keep their APIs.
                if matches!(ty.as_str(), "String" | "i64" | "f64" | "bool") {
                    out.push_str(&format!("impl From<{ty}> for {name} {{ fn from(value: {ty}) -> Self {{ Self(value) }} }}\n"));
                    out.push_str(&format!("impl std::fmt::Display for {name} {{ fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {{ std::fmt::Display::fmt(&self.0, f) }} }}\n"));
                    if ty == "String" {
                        out.push_str(&format!("impl From<&str> for {name} {{ fn from(value: &str) -> Self {{ Self(value.to_owned()) }} }}\n"));
                        out.push_str(&format!("impl AsRef<str> for {name} {{ fn as_ref(&self) -> &str {{ &self.0 }} }}\n"));
                    }
                }
            }
        }
        out.push_str("pub mod commands {\n");
        for command in &domain.commands {
            let wire = command.name.as_str();
            let name = type_name(wire);
            documentation(&command.metadata, out);
            self.structure(domain_name, "", &name, &command.parameters, out);
            self.constructor(domain_name, "", &name, &command.parameters, out);
            self.structure(
                domain_name,
                "",
                &format!("{name}Response"),
                &command.returns,
                out,
            );
            out.push_str(&format!("impl crate::protocol::Command for {name} {{ type Response = {name}Response; const METHOD: &'static str = \"{domain_name}.{wire}\"; }}\n"));
        }
        out.push_str("}\npub mod events {\n");
        for event in &domain.events {
            let wire = event.name.as_str();
            let name = type_name(wire);
            documentation(&event.metadata, out);
            self.structure(domain_name, "", &name, &event.parameters, out);
            out.push_str(&format!("impl crate::protocol::Event for {name} {{ const METHOD: &'static str = \"{domain_name}.{wire}\"; }}\n"));
        }
        out.push_str("}\n}\n");
    }
}

pub(super) fn generate(domains: &[Domain]) -> String {
    let generator = Generator {
        types: Types::new(domains),
    };
    let mut output = String::from("// Generated from protocol/*.json. Do not edit.\n");
    for domain in domains {
        generator.domain(domain, &mut output);
    }
    output
}
