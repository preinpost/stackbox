use crate::{
    farm::FarmNode,
    kolla::Phase,
    model::{Endpoints, Exposure, Plan},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const VERSION: u32 = 1;
pub const MAX_FRAME: usize = 256 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub coordinator_id: String,
    pub operation: Operation,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Hello,
    Scan,
    Create {
        farm_id: String,
        generation: String,
        plan: Plan,
        exposure: Exposure,
    },
    Deploy {
        farm_id: String,
        generation: String,
        from: Phase,
    },
    Delete {
        farm_id: String,
        generation: String,
    },
    Endpoints {
        farm_id: String,
        generation: String,
    },
}
impl Operation {
    pub fn scope(&self) -> Option<(&str, &str)> {
        match self {
            Self::Create {
                farm_id,
                generation,
                ..
            }
            | Self::Deploy {
                farm_id,
                generation,
                ..
            }
            | Self::Delete {
                farm_id,
                generation,
            }
            | Self::Endpoints {
                farm_id,
                generation,
            } => Some((farm_id, generation)),
            _ => None,
        }
    }
    pub fn seconds(&self) -> u64 {
        match self {
            Self::Deploy { .. } => 7200,
            Self::Create { .. } => 600,
            Self::Delete { .. } => 180,
            _ => 60,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    Phase { phase: Phase },
    Output { message: String },
    Result { result: Reply },
    Error { message: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Hello {
        version: u32,
        arch: String,
        #[serde(default)]
        supports_single_vm: bool,
    },
    Inventory {
        farms: BTreeMap<String, Vec<FarmNode>>,
    },
    Nodes {
        nodes: Vec<FarmNode>,
    },
    Endpoints {
        endpoints: Endpoints,
    },
    Done,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_hello_is_readable_without_advertising_the_new_layout() {
        let reply: Reply =
            serde_json::from_str(r#"{"result":"hello","version":1,"arch":"aarch64"}"#).unwrap();
        assert!(matches!(
            reply,
            Reply::Hello {
                supports_single_vm: false,
                ..
            }
        ));
        let artifact: crate::remote::AgentArtifactInfo =
            serde_json::from_str(r#"{"sha256":"legacy","arch":"aarch64","protocol":1}"#).unwrap();
        assert!(!artifact.supports_single_vm);
    }
}
