use heck::{ToSnakeCase, ToUpperCamelCase};
use serde_json::Value;
use std::{collections::HashMap, env, fs, path::PathBuf};

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

fn items<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value[key].as_array().map_or(&[], Vec::as_slice)
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
fn documentation(value: &Value, out: &mut String) {
    if let Some(description) = value["description"].as_str() {
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
    if value["experimental"] == true {
        out.push_str("#[doc = \"\nExperimental in the pinned CDP schema.\"]\n");
    }
    if value["deprecated"] == true {
        out.push_str("#[doc = \"\nDeprecated in the pinned CDP schema.\"]\n");
    }
}

struct Generator {
    // Only edges stored inline can cause infinitely sized Rust types.
    edges: HashMap<String, Vec<String>>,
}

impl Generator {
    fn reference(domain: &str, reference: &str) -> String {
        if reference.contains('.') {
            reference.into()
        } else {
            format!("{domain}.{reference}")
        }
    }

    fn reaches(&self, from: &str, to: &str, seen: &mut Vec<String>) -> bool {
        if from == to {
            return true;
        }
        if seen.iter().any(|key| key == from) {
            return false;
        }
        seen.push(from.into());
        self.edges
            .get(from)
            .is_some_and(|edges| edges.iter().any(|next| self.reaches(next, to, seen)))
    }

    fn rust_type(
        &self,
        domain: &str,
        owner: &str,
        name: &str,
        value: &Value,
        extra: &mut String,
    ) -> String {
        if let Some(reference) = value["$ref"].as_str() {
            let key = Self::reference(domain, reference);
            let (target_domain, target_name) = key.split_once('.').expect("qualified reference");
            let path = format!(
                "crate::cdp::{}::{}",
                ident(target_domain),
                type_name(target_name)
            );
            return if self.reaches(&key, owner, &mut Vec::new()) {
                format!("Box<{path}>")
            } else {
                path
            };
        }
        if value["enum"].is_array() {
            documentation(value, extra);
            extra.push_str(&format!("#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]\npub enum {name} {{\n"));
            let mut used = std::collections::HashSet::new();
            for variant in items(value, "enum") {
                let wire = variant.as_str().expect("string enum");
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
                extra.push_str(&format!("#[serde(rename = {wire:?})]\n{variant_name},\n"));
            }
            extra.push_str("}\n");
            return name.into();
        }
        match value["type"].as_str().expect("schema type") {
            "string" => "String".into(),
            "integer" => "i64".into(),
            "number" => "f64".into(),
            "boolean" => "bool".into(),
            "any" => "serde_json::Value".into(),
            "object" if value["properties"].is_array() => {
                documentation(value, extra);
                self.structure(domain, owner, name, items(value, "properties"), extra);
                name.into()
            }
            "object" => "serde_json::Map<String, serde_json::Value>".into(),
            "array" => {
                // Vec already provides indirection; its elements need no cycle boxing.
                let inner =
                    self.rust_type(domain, "", &format!("{name}Item"), &value["items"], extra);
                format!("Vec<{inner}>")
            }
            other => panic!("unsupported schema type {other}"),
        }
    }

    fn structure(&self, domain: &str, owner: &str, name: &str, fields: &[Value], out: &mut String) {
        let mut extra = String::new();
        let default = if fields.iter().all(|f| f["optional"] == true) {
            ", Default"
        } else {
            ""
        };
        out.push_str(&format!("#[derive(Debug, Clone, serde::Serialize, serde::Deserialize{default})]\npub struct {name} {{\n"));
        for field in fields {
            let wire = field["name"].as_str().expect("field name");
            let mut ty = self.rust_type(
                domain,
                owner,
                &format!("{name}{}", type_name(wire)),
                field,
                &mut extra,
            );
            documentation(field, out);
            out.push_str(&format!("#[serde(rename = {wire:?})]\n"));
            if field["optional"] == true {
                ty = format!("Option<{ty}>");
                out.push_str("#[serde(default, skip_serializing_if = \"Option::is_none\")]\n");
                if field["type"] == "any" {
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
        fields: &[Value],
        out: &mut String,
    ) {
        let types: Vec<_> = fields
            .iter()
            .map(|field| {
                let wire = field["name"].as_str().expect("field name");
                let ty = self.rust_type(
                    domain,
                    owner,
                    &format!("{name}{}", type_name(wire)),
                    field,
                    &mut String::new(),
                );
                (field, ident(wire), ty)
            })
            .collect();
        let required: Vec<_> = types
            .iter()
            .filter(|(field, _, _)| field["optional"] != true)
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
            let value = if field["optional"] == true {
                "None".into()
            } else {
                format!("{member}.into()")
            };
            out.push_str(&format!("{member}: {value},\n"));
        }
        out.push_str("} }\n");
        for (field, member, ty) in &types {
            if field["optional"] == true {
                documentation(field, out);
                out.push_str(&format!("#[must_use]\npub fn with_{member}(mut self, value: impl Into<{ty}>) -> Self {{ self.{member} = Some(value.into()); self }}\n"));
            }
        }
        out.push_str("}\n");
    }

    fn domain(&self, domain: &Value, out: &mut String) {
        let domain_name = domain["domain"].as_str().expect("domain");
        documentation(domain, out);
        out.push_str(&format!("pub mod {} {{\n", ident(domain_name)));
        for value in items(domain, "types") {
            let id = value["id"].as_str().expect("type id");
            let name = type_name(id);
            let owner = format!("{domain_name}.{id}");
            let ty = self.rust_type(domain_name, &owner, &name, value, out);
            if value["type"] == "object" && value["properties"].is_array() {
                self.constructor(domain_name, &owner, &name, items(value, "properties"), out);
            }
            if ty != name {
                documentation(value, out);
                // Transparent newtypes keep protocol identifiers distinct.
                let eq = if value["type"] == "string" || value["type"] == "integer" {
                    ", PartialEq, Eq, Hash"
                } else {
                    ""
                };
                out.push_str(&format!("#[derive(Debug, Clone, serde::Serialize, serde::Deserialize{eq})]\n#[serde(transparent)]\npub struct {name}(pub {ty});\n"));
            }
        }
        out.push_str("pub mod commands {\n");
        for command in items(domain, "commands") {
            let wire = command["name"].as_str().expect("command name");
            let name = type_name(wire);
            documentation(command, out);
            self.structure(domain_name, "", &name, items(command, "parameters"), out);
            self.constructor(domain_name, "", &name, items(command, "parameters"), out);
            self.structure(
                domain_name,
                "",
                &format!("{name}Response"),
                items(command, "returns"),
                out,
            );
            out.push_str(&format!("impl crate::protocol::Command for {name} {{ type Response = {name}Response; const METHOD: &'static str = \"{domain_name}.{wire}\"; }}\n"));
        }
        out.push_str("}\npub mod events {\n");
        for event in items(domain, "events") {
            let wire = event["name"].as_str().expect("event name");
            let name = type_name(wire);
            documentation(event, out);
            self.structure(domain_name, "", &name, items(event, "parameters"), out);
            out.push_str(&format!("impl crate::protocol::Event for {name} {{ const METHOD: &'static str = \"{domain_name}.{wire}\"; }}\n"));
        }
        out.push_str("}\n}\n");
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut domains = Vec::new();
    for path in [
        "protocol/browser_protocol.json",
        "protocol/js_protocol.json",
    ] {
        println!("cargo:rerun-if-changed={path}");
        let schema: Value = serde_json::from_slice(&fs::read(path)?)?;
        domains.extend(items(&schema, "domains").iter().cloned());
    }
    let mut generator = Generator {
        edges: HashMap::new(),
    };
    for domain in &domains {
        let domain_name = domain["domain"].as_str().expect("domain");
        for value in items(domain, "types") {
            let owner = format!("{domain_name}.{}", value["id"].as_str().expect("id"));
            let refs = items(value, "properties")
                .iter()
                .filter_map(|f| f["$ref"].as_str())
                .map(|r| Generator::reference(domain_name, r))
                .collect();
            generator.edges.insert(owner, refs);
        }
    }
    let mut output = String::from("// Generated from protocol/*.json. Do not edit.\n");
    for domain in &domains {
        generator.domain(domain, &mut output);
    }
    fs::write(PathBuf::from(env::var("OUT_DIR")?).join("cdp.rs"), output)?;
    Ok(())
}
