#[path = "../codegen/mod.rs"]
mod codegen;

fn render(schema: serde_json::Value) -> std::io::Result<String> {
    codegen::render(&[("fixture.json", &serde_json::to_vec(&schema).unwrap())])
}

#[test]
fn malformed_parameters_fail_instead_of_emitting_an_empty_command() {
    let error = render(
        serde_json::json!({"domains": [{"domain": "Test", "commands": [
            {"name": "run", "parameters": {"name": "value", "type": "string"}}
        ]}]}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("fixture.json"), "{error}");
    assert!(error.contains("parameters"), "{error}");
}

#[test]
fn incompatible_definitions_and_unresolved_references_fail_before_emission() {
    for (definition, message) in [
        (serde_json::json!({"type": "array"}), "array requires items"),
        (
            serde_json::json!({"type": "string", "$ref": "Missing"}),
            "exactly one",
        ),
        (
            serde_json::json!({"type": "integer", "enum": ["one"]}),
            "enum",
        ),
        (
            serde_json::json!({"type": "string", "properties": []}),
            "properties",
        ),
        (
            serde_json::json!({"type": "object", "properties": null}),
            "invalid type",
        ),
        (
            serde_json::json!({"$ref": "Missing"}),
            "Test.run.parameters.value: unresolved reference",
        ),
    ] {
        let mut field = definition;
        field["name"] = "value".into();
        let error = render(
            serde_json::json!({"domains": [{"domain": "Test", "commands": [
                {"name": "run", "parameters": [field]}
            ]}]}),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(message), "{error}");
    }
}

#[test]
fn pinned_generation_is_deterministic() {
    let inputs = [
        (
            "browser_protocol.json",
            include_bytes!("../protocol/browser_protocol.json").as_slice(),
        ),
        (
            "js_protocol.json",
            include_bytes!("../protocol/js_protocol.json").as_slice(),
        ),
    ];
    assert_eq!(
        codegen::render(&inputs).unwrap(),
        codegen::render(&inputs).unwrap()
    );
}

#[test]
fn enum_duplicates_are_errors_but_empty_wire_values_are_valid() {
    let schema = |variants| {
        serde_json::json!({"domains": [{"domain": "Test", "types": [
            {"id": "Status", "type": "string", "enum": variants}
        ]}]})
    };
    let error = render(schema(vec!["one", "one"])).unwrap_err().to_string();
    assert!(
        error.contains("Test.Status: duplicate enum value"),
        "{error}"
    );
    render(schema(vec!["", "one"])).unwrap();
}

#[test]
fn nested_schema_errors_retain_property_and_item_locations() {
    let error = render(serde_json::json!({"domains": [{"domain": "Test", "types": [
        {"id": "Outer", "type": "object", "properties": [
            {"name": "nested", "type": "array", "items": {"type": "object", "properties": [
                {"name": "broken", "type": "array"}
            ]}}
        ]}
    ]}]}))
    .unwrap_err()
    .to_string();
    assert!(error.contains("domains[0].types[0]"), "{error}");
    assert!(
        error.contains("properties[0]: items: properties[0]: array requires items"),
        "{error}"
    );
}

#[test]
fn aliases_to_any_preserve_present_null_across_domains() {
    let generated = render(serde_json::json!({"domains": [
        {"domain": "Origin", "types": [{"id": "Json", "type": "any"}, {"id": "Alias", "$ref": "Json"}]},
        {"domain": "Consumer", "commands": [{"name": "send", "parameters": [
            {"name": "payload", "$ref": "Origin.Alias", "optional": true}
        ]}]}
    ]})).unwrap();
    assert!(generated.contains("deserialize_with = \"crate::protocol::present_json\""));
}

#[test]
fn recursive_aliases_and_nested_objects_are_boxed_but_vectors_are_not() {
    let generated = render(serde_json::json!({"domains": [{"domain": "Test", "types": [
        {"id": "Alias", "$ref": "Node"},
        {"id": "Node", "type": "object", "properties": [
            {"name": "nested", "type": "object", "properties": [
                {"name": "parent", "$ref": "Alias", "optional": true}
            ]},
            {"name": "children", "type": "array", "items": {"$ref": "Node"}}
        ]}
    ]}]}))
    .unwrap();
    let generated = generated.split_whitespace().collect::<String>();
    assert!(generated.contains("pubstructAlias(pubBox<crate::cdp::test::Node>)"));
    assert!(generated.contains("pubparent:Option<Box<crate::cdp::test::Alias>>"));
    assert!(generated.contains("pubchildren:Vec<crate::cdp::test::Node>"));
}
