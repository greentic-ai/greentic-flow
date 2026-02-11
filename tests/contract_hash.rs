use greentic_flow::contracts;
use greentic_types::schemas::common::schema_ir::{AdditionalProperties, SchemaIr};
use greentic_types::schemas::component::v0_6_0::{
    ComponentDescribe, ComponentInfo, ComponentOperation, ComponentRunInput, ComponentRunOutput,
    schema_hash,
};
use std::collections::BTreeMap;

#[test]
fn recompute_schema_hash_matches() {
    let config_schema = SchemaIr::Object {
        properties: BTreeMap::new(),
        required: Vec::new(),
        additional: AdditionalProperties::Allow,
    };
    let op_schema = SchemaIr::Object {
        properties: BTreeMap::new(),
        required: Vec::new(),
        additional: AdditionalProperties::Allow,
    };
    let expected = schema_hash(&op_schema, &op_schema, &config_schema).unwrap();
    let op = ComponentOperation {
        id: "run".to_string(),
        display_name: None,
        input: ComponentRunInput {
            schema: op_schema.clone(),
        },
        output: ComponentRunOutput { schema: op_schema },
        defaults: BTreeMap::new(),
        redactions: Vec::new(),
        constraints: BTreeMap::new(),
        schema_hash: expected.clone(),
    };
    let recomputed = contracts::recompute_schema_hash(&op, &config_schema).unwrap();
    assert_eq!(recomputed, expected);
}

#[test]
fn describe_hash_is_deterministic() {
    let describe = ComponentDescribe {
        info: ComponentInfo {
            id: "acme.widget".to_string(),
            version: "0.1.0".to_string(),
            role: "tool".to_string(),
            display_name: None,
        },
        provided_capabilities: Vec::new(),
        required_capabilities: Vec::new(),
        metadata: BTreeMap::new(),
        operations: Vec::new(),
        config_schema: SchemaIr::Object {
            properties: BTreeMap::new(),
            required: Vec::new(),
            additional: AdditionalProperties::Allow,
        },
    };
    let hash1 = contracts::describe_hash(&describe).unwrap();
    let hash2 = contracts::describe_hash(&describe).unwrap();
    assert_eq!(hash1, hash2);
}
