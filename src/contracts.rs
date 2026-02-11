use anyhow::{Result, anyhow};
use greentic_types::cbor::canonical;
use greentic_types::schemas::component::v0_6_0::{
    ComponentDescribe, ComponentOperation, schema_hash,
};
use sha2::{Digest, Sha256};

pub fn decode_component_describe(bytes: &[u8]) -> Result<ComponentDescribe> {
    if bytes.is_empty() {
        return Err(anyhow!("describe() returned empty payload"));
    }
    let describe: ComponentDescribe =
        canonical::from_cbor(bytes).map_err(|err| anyhow!("decode describe cbor: {err}"))?;
    Ok(describe)
}

pub fn describe_hash(describe: &ComponentDescribe) -> Result<String> {
    let bytes = canonical::to_canonical_cbor_allow_floats(describe)
        .map_err(|err| anyhow!("encode describe for hashing: {err}"))?;
    let digest = Sha256::digest(bytes.as_slice());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok(hex)
}

pub fn find_operation<'a>(
    describe: &'a ComponentDescribe,
    operation_id: &str,
) -> Result<&'a ComponentOperation> {
    for op in &describe.operations {
        if op.id == operation_id {
            return Ok(op);
        }
    }
    Err(anyhow!(
        "operation '{}' not found in describe() payload",
        operation_id
    ))
}

pub fn recompute_schema_hash(
    op: &ComponentOperation,
    config_schema: &greentic_types::schemas::common::schema_ir::SchemaIr,
) -> Result<String> {
    schema_hash(&op.input.schema, &op.output.schema, config_schema)
        .map_err(|err| anyhow!("compute schema hash: {err}"))
}
