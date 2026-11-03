use crate::{
    farm::{FarmNode, FrozenSnapshot, ImportedSnapshot, TerminalSize},
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
    /// Runs as the host-side job `job_id`, which outlives this request; repeating the
    /// request with the same `job_id` attaches to that job instead of deploying again.
    Deploy {
        farm_id: String,
        generation: String,
        from: Phase,
        job_id: String,
    },
    /// Follow a host-side job this coordinator started, skipping the first `offset` progress
    /// frames it already received, until the job's result.
    Attach {
        job_id: String,
        offset: u64,
    },
    Delete {
        farm_id: String,
        generation: String,
    },
    Endpoints {
        farm_id: String,
        generation: String,
    },
    /// Wait for a deployed farm's services after its VM started from disk, first moving
    /// the public catalog entries from `rebind.0` to `rebind.1`.
    Settle {
        farm_id: String,
        generation: String,
        #[serde(default)]
        rebind: Option<(Exposure, Exposure)>,
    },
    /// Stop the farm's VM, capture its disks as `snapshot_id`, and start it again.
    Freeze {
        farm_id: String,
        generation: String,
        snapshot_id: String,
        labels: BTreeMap<String, String>,
    },
    /// Set the faults of a multi-node farm VM's links (`node` is `control`, `compute-1`, ...);
    /// no delay and no loss heals them.
    Impair {
        farm_id: String,
        generation: String,
        node: String,
        impair: crate::link::Impair,
    },
    /// Reserve like `Create`, but boot the farm's VM from a snapshot on this host.
    Restore {
        farm_id: String,
        generation: String,
        reference: String,
        /// A multi-node snapshot's VMs.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        nodes: Vec<crate::farm::NodeSnapshot>,
        plan: Plan,
        exposure: Exposure,
    },
    /// Write a snapshot to an archive file on this host.
    ExportSnapshot {
        reference: String,
        output: String,
    },
    /// Load an archive file on this host as a snapshot.
    ImportSnapshot {
        archive: String,
        snapshot_id: String,
    },
    RemoveSnapshot {
        reference: String,
    },
    /// Interactive shell in the farm's VM. After the request line, the coordinator keeps
    /// stdin open and writes one `TerminalFrame` per line until the session ends.
    Shell {
        farm_id: String,
        generation: String,
        size: TerminalSize,
    },
    /// Report this host's OS, hypervisor, msb, golden image and resources.
    Inspect,
    /// Boot a small VM from the golden image's base image, run a command in it and remove it.
    VerifyVm,
    /// Install the coordinator's msb release, upgrading an older one; a newer one fails.
    InstallMsb,
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
            }
            | Self::Settle {
                farm_id,
                generation,
                ..
            }
            | Self::Freeze {
                farm_id,
                generation,
                ..
            }
            | Self::Impair {
                farm_id,
                generation,
                ..
            }
            | Self::Restore {
                farm_id,
                generation,
                ..
            } => Some((farm_id, generation)),
            _ => None,
        }
    }
    pub fn seconds(&self) -> u64 {
        match self {
            // An attach follows a deploy, which stops itself after the same time.
            Self::Deploy { .. } | Self::Attach { .. } => 7200,
            // The host's first farm also builds the golden image, mostly package downloads.
            Self::Create { .. } => 1800,
            Self::Delete { .. } => 180,
            // A host can hold several farms, each destroyed VM by VM.
            Self::Purge => 1800,
            Self::Shell { .. } => 12 * 3600,
            Self::Settle { .. } | Self::Freeze { .. } | Self::Restore { .. } => 3600,
            // Each moves a whole VM disk.
            Self::ExportSnapshot { .. } | Self::ImportSnapshot { .. } => 7200,
            Self::RemoveSnapshot { .. } => 300,
            Self::VerifyVm => 300,
            // Downloads the release bundle; the script itself stops after 600 s.
            Self::InstallMsb => 660,
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
    Frozen {
        snapshot: FrozenSnapshot,
    },
    Inspection {
        inspection: super::HostInspection,
    },
    Imported {
        snapshot: ImportedSnapshot,
    },
    Done,
    Purged {
        /// Farms whose VMs were destroyed, including ones already deleted from the registry.
        farms: usize,
    },
}

impl Operation {
    /// Whether the agent reports phases and steps while running this operation.
    fn reports_progress(&self) -> bool {
        matches!(
            self,
            Self::Deploy { .. } | Self::Attach { .. } | Self::Settle { .. }
        )
    }
    /// Whether the agent may stream command output while running this operation.
    fn reports_output(&self) -> bool {
        !matches!(
            self,
            Self::Endpoints { .. } | Self::Hello | Self::Scan | Self::Inspect
        )
    }
}

/// An operation's own failure as the agent reported it. Any other error of a request means
/// the agent was not reached or the connection to it broke, so the outcome is unknown.
#[derive(Debug)]
pub struct AgentError(pub String);
impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for AgentError {}

/// Hand a progress frame of a one-shot request to `sink`. Returns the outcome once the
/// agent reports one; a frame the operation cannot produce is a protocol error.
pub fn deliver(
    frame: Frame,
    operation: &Operation,
    sink: Option<&crate::kolla::EventSink>,
) -> anyhow::Result<Option<anyhow::Result<Reply>>> {
    use crate::kolla::DeployEvent;
    let event = match frame {
        Frame::Result { result } => return Ok(Some(Ok(result))),
        Frame::Error { message } => return Ok(Some(Err(AgentError(message).into()))),
        Frame::Attached | Frame::Terminal { .. } => {
            anyhow::bail!("unexpected farm-agent terminal event")
        }
        Frame::Phase { phase } if operation.reports_progress() => DeployEvent::Phase(phase),
        Frame::Step { step } if operation.reports_progress() => DeployEvent::Step(step),
        Frame::Output { message } if operation.reports_output() => DeployEvent::Output(message),
        _ => anyhow::bail!("unexpected farm-agent progress event"),
    };
    if let Some(sink) = sink {
        sink(event)?;
    }
    Ok(None)
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
