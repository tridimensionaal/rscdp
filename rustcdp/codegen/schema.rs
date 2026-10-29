use serde::{Deserialize, Deserializer};
use std::{
    collections::{HashMap, HashSet},
    io,
};

#[derive(Debug, Deserialize)]
pub struct Protocol {
    pub domains: Vec<Domain>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Metadata {
    pub description: Option<String>,
    #[serde(default)]
    pub experimental: bool,
    #[serde(default)]
    pub deprecated: bool,
}

#[derive(Debug, Deserialize)]
pub struct Domain {
    pub domain: String,
    #[serde(flatten)]
    pub metadata: Metadata,
    #[serde(default)]
    pub types: Vec<NamedType>,
    #[serde(default)]
    pub commands: Vec<Command>,
    #[serde(default)]
    pub events: Vec<Event>,
}

#[derive(Debug, Deserialize)]
pub struct NamedType {
    pub id: String,
    #[serde(flatten)]
    pub definition: Definition,
}

#[derive(Debug, Deserialize)]
pub struct Command {
    pub name: String,
    #[serde(flatten)]
    pub metadata: Metadata,
    #[serde(default)]
    pub parameters: Vec<Field>,
    #[serde(default)]
    pub returns: Vec<Field>,
}

#[derive(Debug, Deserialize)]
pub struct Event {
    pub name: String,
    #[serde(flatten)]
    pub metadata: Metadata,
    #[serde(default)]
    pub parameters: Vec<Field>,
}

#[derive(Debug, Deserialize)]
pub struct Field {
    pub name: String,
    #[serde(default)]
    pub optional: bool,
    #[serde(flatten)]
    pub definition: Definition,
}

#[derive(Debug)]
pub struct Definition {
    pub metadata: Metadata,
    pub kind: Kind,
}

#[derive(Debug)]
pub enum Kind {
    Reference(String),
    String(Option<Vec<String>>),
    Integer,
    Number,
    Boolean,
    Any,
    Object(Option<Vec<Field>>),
    Array(Box<Definition>),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Primitive {
    String,
    Integer,
    Number,
    Boolean,
    Any,
    Object,
    Array,
}

// Missing shape members are allowed where appropriate; explicit null is not
// interchangeable with omission for schema structure such as properties/items.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Deserialize)]
struct RawDefinition {
    #[serde(flatten)]
    metadata: Metadata,
    #[serde(rename = "$ref", default, deserialize_with = "present")]
    reference: Option<String>,
    #[serde(rename = "type", default, deserialize_with = "present")]
    primitive: Option<Primitive>,
    #[serde(rename = "enum", default, deserialize_with = "present")]
    variants: Option<Vec<String>>,
    #[serde(default, deserialize_with = "present")]
    properties: Option<Vec<Field>>,
    #[serde(default, deserialize_with = "present")]
    items: Option<Box<Definition>>,
}

impl<'de> Deserialize<'de> for Definition {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Flattened definitions are buffered by Serde. Track their nested path
        // separately, using one concrete deserializer type for recursive shapes.
        let value = serde_json::Value::deserialize(deserializer)?;
        let raw: RawDefinition =
            serde_path_to_error::deserialize(value).map_err(serde::de::Error::custom)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<RawDefinition> for Definition {
    type Error = String;

    fn try_from(raw: RawDefinition) -> Result<Self, Self::Error> {
        if raw.properties.is_some() && !matches!(raw.primitive, Some(Primitive::Object)) {
            return Err("properties is only supported on object types".into());
        }
        if raw.items.is_some() && !matches!(raw.primitive, Some(Primitive::Array)) {
            return Err("items is only supported on array types".into());
        }
        let kind = match (raw.reference, raw.primitive) {
            (Some(reference), None) => Kind::Reference(reference),
            (None, Some(Primitive::String)) => Kind::String(raw.variants.clone()),
            (None, Some(Primitive::Integer)) => Kind::Integer,
            (None, Some(Primitive::Number)) => Kind::Number,
            (None, Some(Primitive::Boolean)) => Kind::Boolean,
            (None, Some(Primitive::Any)) => Kind::Any,
            (None, Some(Primitive::Object)) => Kind::Object(raw.properties),
            (None, Some(Primitive::Array)) => Kind::Array(raw.items.ok_or("array requires items")?),
            _ => return Err("definition requires exactly one of type or $ref".into()),
        };
        if raw.variants.is_some() && !matches!(kind, Kind::String(_)) {
            return Err("enum is only supported on string types".into());
        }
        Ok(Self {
            metadata: raw.metadata,
            kind,
        })
    }
}

pub fn parse(bytes: &[u8], source: &str) -> io::Result<Protocol> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let protocol = serde_path_to_error::deserialize(&mut deserializer)
        .map_err(|error| invalid(format!("{source}: {error}")))?;
    deserializer
        .end()
        .map_err(|error| invalid(format!("{source}: {error}")))?;
    Ok(protocol)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub fn reference(domain: &str, name: &str) -> String {
    if name.contains('.') {
        name.to_owned()
    } else {
        format!("{domain}.{name}")
    }
}

fn unique_names<'a>(names: impl Iterator<Item = &'a str>, location: &str) -> io::Result<()> {
    let mut seen = HashSet::new();
    for name in names {
        if name.is_empty() || !seen.insert(name) {
            return Err(invalid(format!(
                "{location}: empty or duplicate name {name:?}"
            )));
        }
    }
    Ok(())
}

pub fn validate(domains: &[Domain]) -> io::Result<()> {
    unique_names(domains.iter().map(|d| d.domain.as_str()), "domains")?;
    let mut types = HashMap::new();
    for domain in domains {
        unique_names(domain.types.iter().map(|t| t.id.as_str()), &domain.domain)?;
        for ty in &domain.types {
            types.insert(reference(&domain.domain, &ty.id), &ty.definition);
        }
    }
    for domain in domains {
        let name = &domain.domain;
        for ty in &domain.types {
            validate_definition(&ty.definition, name, &reference(name, &ty.id), &types)?;
        }
        unique_names(
            domain.commands.iter().map(|c| c.name.as_str()),
            &format!("{name}.commands"),
        )?;
        unique_names(
            domain.events.iter().map(|e| e.name.as_str()),
            &format!("{name}.events"),
        )?;
        for command in &domain.commands {
            validate_fields(
                &command.parameters,
                name,
                &format!("{name}.{}.parameters", command.name),
                &types,
            )?;
            validate_fields(
                &command.returns,
                name,
                &format!("{name}.{}.returns", command.name),
                &types,
            )?;
        }
        for event in &domain.events {
            validate_fields(
                &event.parameters,
                name,
                &format!("{name}.{}.parameters", event.name),
                &types,
            )?;
        }
    }
    Ok(())
}

fn validate_fields(
    fields: &[Field],
    domain: &str,
    location: &str,
    types: &HashMap<String, &Definition>,
) -> io::Result<()> {
    unique_names(fields.iter().map(|f| f.name.as_str()), location)?;
    for field in fields {
        validate_definition(
            &field.definition,
            domain,
            &format!("{location}.{}", field.name),
            types,
        )?;
    }
    Ok(())
}

fn validate_definition(
    definition: &Definition,
    domain: &str,
    location: &str,
    types: &HashMap<String, &Definition>,
) -> io::Result<()> {
    match &definition.kind {
        Kind::Reference(name) if !types.contains_key(&reference(domain, name)) => Err(invalid(
            format!("{location}: unresolved reference {name:?}"),
        )),
        Kind::String(Some(variants)) => {
            let mut seen = HashSet::new();
            for variant in variants {
                if !seen.insert(variant) {
                    return Err(invalid(format!(
                        "{location}: duplicate enum value {variant:?}"
                    )));
                }
            }
            Ok(())
        }
        Kind::Object(Some(fields)) => validate_fields(fields, domain, location, types),
        Kind::Array(inner) => {
            validate_definition(inner, domain, &format!("{location}.items"), types)
        }
        _ => Ok(()),
    }
}
