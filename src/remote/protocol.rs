use crate::{
    farm::{FarmNode, TerminalSize},
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
    /// Interactive shell in the farm's VM. After the request line, the coordinator keeps
    /// stdin open and writes one `TerminalFrame` per line until the session ends.
    Shell {
        farm_id: String,
        generation: String,
        size: TerminalSize,
    },
    /// Destroy every VM this coordinator reserved on the host, then forget them.
    /// The coordinator runs this only when removing the host.
    Purge,
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
            }
            | Self::Shell {
                farm_id,
                generation,
                ..
            } => Some((farm_id, generation)),
            _ => None,
        }
    }
    pub fn seconds(&self) -> u64 {
        match self {
            Self::Deploy { .. } => 7200,
            Self::Create { .. } => 600,
            Self::Delete { .. } => 180,
            // A host can hold several farms, each destroyed VM by VM.
            Self::Purge => 1800,
            Self::Shell { .. } => 12 * 3600,
            _ => 60,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    Phase {
        phase: Phase,
    },
    /// A finer step inside the phase, forwarded to the coordinator's farm record.
    Step {
        step: String,
    },
    Output {
        message: String,
    },
    /// The shell is running; terminal output follows.
    Attached,
    /// Base64 terminal output.
    Terminal {
        data: String,
    },
    Result {
        result: Reply,
    },
    Error {
        message: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Hello {
        version: u32,
        arch: String,
        #[serde(default)]
        supports_single_vm: bool,
        /// Revision the installed agent was built from; empty for agents older than this field.
        #[serde(default)]
        build_id: String,
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
    Purged {
        /// Farms whose VMs were destroyed, including ones already deleted from the registry.
        farms: usize,
    },
}

/// Coordinator-to-agent input of a shell session, one JSON object per line.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalFrame {
    /// Base64 keystrokes.
    Input {
        data: String,
    },
    Resize {
        rows: u16,
        cols: u16,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn new_frames_and_replies_round_trip() {
        let step = serde_json::to_string(&Frame::Step {
            step: "TASK [nova : Copy config]".into(),
        })
        .unwrap();
        assert!(matches!(
            serde_json::from_str::<Frame>(&step).unwrap(),
            Frame::Step { ref step } if step.contains("nova")
        ));
        let purged: Reply = serde_json::from_str(r#"{"result":"purged","farms":3}"#).unwrap();
        assert!(matches!(purged, Reply::Purged { farms: 3 }));
    }

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
