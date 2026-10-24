use hudson_core::{definitions, Error};
use serde_json::{json, Value};

#[test]
fn literal_reference_keys_remain_instance_data() {
    let literal =
        json!({"$ref":"https://example.invalid/data", "$dynamicRef":"file:///not-a-resource"});
    for schema in [json!({"const":literal}), json!({"enum":[literal]})] {
        definitions::validate_schema(&schema).unwrap();
        definitions::validate(&schema, &literal).unwrap();
        assert!(definitions::validate(&schema, &json!({"$ref":"different"})).is_err());
    }
}

#[test]
fn annotations_and_custom_data_do_not_become_schemas() {
    for keyword in ["default", "examples", "x-metadata"] {
        let data = json!({"$ref":"https://example.invalid/annotation"});
        let value = if keyword == "examples" {
            json!([data])
        } else {
            data
        };
        let schema = json!({"type":"object", keyword:value});
        definitions::validate_schema(&schema).unwrap();
        definitions::validate(&schema, &json!({})).unwrap();
    }
}

#[test]
fn nested_literals_and_internal_references_keep_their_meaning() {
    let schema = json!({
        "$defs":{"literal":{"const":{"$ref":"https://example.invalid/literal"}}},
        "type":"object",
        "properties":{"value":{"$ref":"#/$defs/literal"}},
        "required":["value"]
    });
    definitions::validate_schema(&schema).unwrap();
    definitions::validate(
        &schema,
        &json!({"value":{"$ref":"https://example.invalid/literal"}}),
    )
    .unwrap();
    assert!(definitions::validate(&schema, &json!({"value":{}})).is_err());
}

fn assert_external_reference_denied(schema: Value) {
    assert!(
        matches!(definitions::validate_schema(&schema), Err(Error::Invalid(message)) if message == "external schema references are disabled")
    );
}

#[test]
fn actual_references_are_denied_in_every_supported_schema_container() {
    let reference = json!({"$ref":"https://example.invalid/resource"});
    assert_external_reference_denied(reference.clone());
    assert_external_reference_denied(json!({"$dynamicRef":"https://example.invalid/resource"}));
    for keyword in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
        "dependencies",
    ] {
        for name in ["const", "enum", "examples", "$ref", "default"] {
            assert_external_reference_denied(json!({keyword:{name:reference}}));
        }
    }
    for keyword in ["allOf", "anyOf", "oneOf", "prefixItems", "items"] {
        assert_external_reference_denied(json!({keyword:[reference]}));
    }
    for keyword in [
        "items",
        "additionalItems",
        "additionalProperties",
        "unevaluatedItems",
        "unevaluatedProperties",
        "contains",
        "not",
        "propertyNames",
        "if",
        "then",
        "else",
        "contentSchema",
    ] {
        assert_external_reference_denied(json!({keyword:reference}));
    }
}

#[test]
fn a_locally_embedded_id_cannot_admit_an_external_reference() {
    // Even an already present resource must use a local fragment reference.
    assert_external_reference_denied(json!({
        "$defs":{"target":{"$id":"https://example.invalid/embedded","type":"string"}},
        "properties":{"const":{"$ref":"https://example.invalid/embedded"}}
    }));
}
