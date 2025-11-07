use crate::model::FlowDoc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowBundleVersion {
    V1,
}

#[derive(Debug, Clone)]
pub struct FlowBundle {
    pub version: FlowBundleVersion,
    pub flow: FlowDoc,
}

impl FlowBundle {
    pub fn new(flow: FlowDoc) -> Self {
        FlowBundle {
            version: FlowBundleVersion::V1,
            flow,
        }
    }

    pub fn flow(&self) -> &FlowDoc {
        &self.flow
    }

    pub fn into_flow(self) -> FlowDoc {
        self.flow
    }
}
