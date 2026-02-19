use greentic_interfaces::canonical::node::{ComponentDescriptor, SchemaSource, SetupOutput};

pub fn has_setup(descriptor: &ComponentDescriptor) -> bool {
    descriptor.setup.is_some()
}

pub fn qa_spec_ref(descriptor: &ComponentDescriptor) -> Option<&SchemaSource> {
    descriptor.setup.as_ref().map(|setup| &setup.qa_spec)
}

pub fn answers_schema_ref(descriptor: &ComponentDescriptor) -> Option<&SchemaSource> {
    descriptor.setup.as_ref().map(|setup| &setup.answers_schema)
}

pub fn setup_outputs(descriptor: &ComponentDescriptor) -> Option<&[SetupOutput]> {
    descriptor
        .setup
        .as_ref()
        .map(|setup| setup.outputs.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use greentic_interfaces::canonical::node::{IoSchema, Op, SchemaRef, SetupContract};

    fn sample_descriptor() -> ComponentDescriptor {
        ComponentDescriptor {
            name: "demo".to_string(),
            version: "0.1.0".to_string(),
            summary: Some("demo component".to_string()),
            capabilities: vec!["host:config".to_string()],
            ops: vec![Op {
                name: "setup.apply_answers".to_string(),
                summary: None,
                input: IoSchema {
                    schema: SchemaSource::InlineCbor(vec![0xa0]),
                    content_type: "application/cbor".to_string(),
                    schema_version: None,
                },
                output: IoSchema {
                    schema: SchemaSource::InlineCbor(vec![0xa0]),
                    content_type: "application/cbor".to_string(),
                    schema_version: None,
                },
                examples: Vec::new(),
            }],
            schemas: vec![SchemaRef {
                id: "schema-1".to_string(),
                content_type: "application/cbor".to_string(),
                blake3_hash: "deadbeef".to_string(),
                version: "1".to_string(),
                bytes: None,
                uri: None,
            }],
            setup: Some(SetupContract {
                qa_spec: SchemaSource::InlineCbor(vec![0xa1, 0x01]),
                answers_schema: SchemaSource::InlineCbor(vec![0xa1, 0x02]),
                examples: Vec::new(),
                outputs: vec![SetupOutput::ConfigOnly],
            }),
        }
    }

    #[test]
    fn extracts_setup_refs() {
        let descriptor = sample_descriptor();
        assert!(has_setup(&descriptor));
        assert!(matches!(
            qa_spec_ref(&descriptor),
            Some(SchemaSource::InlineCbor(bytes)) if bytes == &vec![0xa1, 0x01]
        ));
        assert!(matches!(
            answers_schema_ref(&descriptor),
            Some(SchemaSource::InlineCbor(bytes)) if bytes == &vec![0xa1, 0x02]
        ));
        assert!(matches!(
            setup_outputs(&descriptor),
            Some(outputs) if outputs.len() == 1
        ));
    }
}
