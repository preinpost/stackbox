use crate::{
    kolla::Phase,
    model::{Endpoints, Exposure, Plan},
    stackbox::{FrozenSnapshot, ImportedSnapshot, StackboxNode, TerminalSize},
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
    /// The public port bases this host reserves for stackboxes of every coordinator.
    PublicPorts,
    Create {
        stackbox_id: String,
        generation: String,
        plan: Plan,
        exposure: Exposure,
        /// For a stackbox that spans hosts: the part this host runs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        share: Option<Box<Share>>,
        /// Compute template snapshot on this host to boot the share's one compute from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Ready this host's compute VMs of a stackbox that spans hosts for Kolla, which the
    /// control VM's host deploys afterwards.
    Prepare {
        stackbox_id: String,
        generation: String,
    },
    /// Add a compute VM to an existing multi-node stackbox on this host, or update the
    /// reservation's plan and the VMs this host runs after another host gained one.
    /// `uplink` is set when the control VM's host joins the relay for the first time.
    Expand {
        stackbox_id: String,
        generation: String,
        plan: Plan,
        /// Suffixes of the stackbox's VMs this host runs after the change.
        nodes: Vec<String>,
        /// Suffixes to create here; the others already exist or run elsewhere.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        add: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uplink: Option<crate::link::UplinkConfig>,
        /// Compute template snapshot on this host to boot the added VM from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Return the SSH key pair the control VM uses to reach the stackbox's computes.
    SshKeys {
        stackbox_id: String,
        generation: String,
    },
    /// Deploy one new compute VM of a multi-node stackbox with Kolla, limited to that VM.
    DeployCompute {
        stackbox_id: String,
        generation: String,
        index: u8,
        /// The VM was booted from a compute template, so Kolla only reconfigures it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        from_template: bool,
    },
    /// Point this host's switch at the relay's new address for a stackbox that spans hosts.
    Relink {
        stackbox_id: String,
        generation: String,
        url: String,
    },
    /// Runs as the host-side job `job_id`, which outlives this request; repeating the
    /// request with the same `job_id` attaches to that job instead of deploying again.
    Deploy {
        stackbox_id: String,
        generation: String,
        from: Phase,
        job_id: String,
        /// The kolla-ansible ref the coordinator resolved for the stackbox's release, such as
        /// `unmaintained/2025.1`. The agent's own process never reads the release catalog,
        /// so a coordinator built before this field sends none and the install script falls
        /// back to `stable/<release>`. An older agent never sees such a request: the
        /// coordinator checks the agent's build first, and `deny_unknown_fields` would
        /// refuse the field anyway.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
    },
    /// Follow a host-side job this coordinator started, skipping the first `offset` progress
    /// frames it already received, until the job's result.
    Attach {
        job_id: String,
        offset: u64,
    },
    Delete {
        stackbox_id: String,
        generation: String,
    },
    Endpoints {
        stackbox_id: String,
        generation: String,
    },
    /// Wait for a deployed stackbox's services after its VM started from disk, first moving
    /// the public catalog entries from `rebind.0` to `rebind.1`.
    Settle {
        stackbox_id: String,
        generation: String,
        #[serde(default)]
        rebind: Option<(Exposure, Exposure)>,
    },
    /// Drop the registrations of the source stackbox's computes from a stackbox restored from a
    /// control template, and list no compute in its Kolla inventory.
    ForgetComputes {
        stackbox_id: String,
        generation: String,
    },
    /// Give a restored stackbox's compute-1 the PCI devices its reservation recorded (see
    /// `kolla::attach_pci`), once its services answer.
    AttachPci {
        stackbox_id: String,
        generation: String,
    },
    /// Stop the stackbox's VM, capture its disks as `snapshot_id`, and start it again. With
    /// `node`, only that VM is captured, as a control or compute template.
    Freeze {
        stackbox_id: String,
        generation: String,
        snapshot_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node: Option<String>,
        labels: BTreeMap<String, String>,
    },
    /// Set the faults of a multi-node stackbox VM's links (`node` is `control`, `compute-1`, ...);
    /// no delay and no loss heals them.
    Impair {
        stackbox_id: String,
        generation: String,
        node: String,
        impair: crate::link::Impair,
    },
    /// Reserve like `Create`, but boot the stackbox's VM from a snapshot on this host.
    Restore {
        stackbox_id: String,
        generation: String,
        reference: String,
        /// A multi-node snapshot's VMs.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        nodes: Vec<crate::stackbox::NodeSnapshot>,
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
    /// Whether this host keeps the coordinator's snapshot `snapshot_id`, which an earlier
    /// `ImportArchive` cached here; the agent checks that msb still has it.
    FindSnapshot {
        snapshot_id: String,
    },
    /// Download a snapshot archive from a presigned bucket URL into the staging file of
    /// transfer `transfer_id`, which must end up `size_bytes` long with SHA-256 `sha256`.
    /// The URL grants reading that one object until it expires; it is never stored.
    FetchArchive {
        transfer_id: String,
        url: String,
        /// PEM certificates of a bucket behind a private CA.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ca_pem: Option<String>,
        sha256: String,
        size_bytes: u64,
    },
    /// Receive a snapshot archive from the coordinator into transfer `transfer_id`'s staging
    /// file. Like a shell, stdin stays open after the request: the archive follows as
    /// `TerminalFrame::Input` lines until EOF, then the agent checks its size and SHA-256.
    ReceiveArchive {
        transfer_id: String,
        sha256: String,
        size_bytes: u64,
    },
    /// Load transfer `transfer_id`'s verified staging file as the snapshot `snapshot_id`,
    /// remember it for `FindSnapshot`, and delete the file.
    ImportArchive {
        transfer_id: String,
        snapshot_id: String,
    },
    /// Write the snapshot at `reference` into transfer `transfer_id`'s staging file and
    /// report its SHA-256 and size, so `SendArchive` can stream it to the coordinator.
    ExportArchive {
        transfer_id: String,
        reference: String,
    },
    /// Stream transfer `transfer_id`'s staging file to the coordinator as `Frame::Terminal`
    /// output, then delete it. Like a shell, stdin stays open until the session ends, but
    /// nothing is read from it.
    SendArchive {
        transfer_id: String,
    },
    /// Interactive shell in the stackbox's VM. After the request line, the coordinator keeps
    /// stdin open and writes one `TerminalFrame` per line until the session ends.
    Shell {
        stackbox_id: String,
        generation: String,
        /// A multi-node VM's suffix (`control`, `compute-1`); absent for the default VM.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node: Option<String>,
        size: TerminalSize,
    },
    /// Report this host's OS, hypervisor, msb, golden image and resources.
    Inspect,
    /// Boot a small VM from the golden image's base image, run a command in it and remove it.
    VerifyVm,
    /// Install the coordinator's msb release, upgrading an older one; a newer one fails.
    InstallMsb,
    /// Destroy every VM on the host, whichever coordinator reserved it, then forget them.
    /// The coordinator sends this only when removing the host with `all_vms`.
    Purge,
    /// Destroy only the VMs this coordinator reserved on the host, then forget them. Other
    /// coordinators' reservations stay, and the reply counts them. An older agent does not
    /// know this operation and refuses it, so the removal fails rather than deleting others' VMs.
    PurgeOwn,
    /// Delete every golden snapshot group this host holds for stackboxes, under the golden build
    /// lock so a build in progress finishes first. Sent before the host's files are removed.
    RemoveGolden,
    /// Bind one PCI device to `vfio-pci` through the host's stackbox-vfio-bind helper, then
    /// reply with the host's fresh inspection.
    PciBind {
        bdf: String,
    },
    /// Give one PCI device back to its host driver through the helper, then reply with the
    /// host's fresh inspection.
    PciUnbind {
        bdf: String,
    },
}
impl Operation {
    pub fn scope(&self) -> Option<(&str, &str)> {
        match self {
            Self::Create {
                stackbox_id,
                generation,
                ..
            }
            | Self::Deploy {
                stackbox_id,
                generation,
                ..
            }
            | Self::Delete {
                stackbox_id,
                generation,
            }
            | Self::Prepare {
                stackbox_id,
                generation,
            }
            | Self::Expand {
                stackbox_id,
                generation,
                ..
            }
            | Self::SshKeys {
                stackbox_id,
                generation,
            }
            | Self::DeployCompute {
                stackbox_id,
                generation,
                ..
            }
            | Self::Relink {
                stackbox_id,
                generation,
                ..
            }
            | Self::Endpoints {
                stackbox_id,
                generation,
            }
            | Self::Shell {
                stackbox_id,
                generation,
                ..
            }
            | Self::Settle {
                stackbox_id,
                generation,
                ..
            }
            | Self::ForgetComputes {
                stackbox_id,
                generation,
            }
            | Self::AttachPci {
                stackbox_id,
                generation,
            }
            | Self::Freeze {
                stackbox_id,
                generation,
                ..
            }
            | Self::Impair {
                stackbox_id,
                generation,
                ..
            }
            | Self::Restore {
                stackbox_id,
                generation,
                ..
            } => Some((stackbox_id, generation)),
            _ => None,
        }
    }
    pub fn seconds(&self) -> u64 {
        match self {
            // An attach follows a deploy, which stops itself after the same time.
            Self::Deploy { .. } | Self::Attach { .. } => 7200,
            // The host's first stackbox also builds the golden image, mostly package downloads.
            Self::Create { .. } => 1800,
            Self::Delete { .. } => 180,
            // A host can hold several stackboxes, each destroyed VM by VM.
            Self::Purge | Self::PurgeOwn => 1800,
            // Each group is a snapshot set of a golden VM; a build that holds the lock finishes first.
            Self::RemoveGolden => 1800,
            Self::Shell { .. } => 12 * 3600,
            Self::Settle { .. }
            | Self::ForgetComputes { .. }
            | Self::AttachPci { .. }
            | Self::Freeze { .. }
            | Self::Restore { .. }
            | Self::Prepare { .. }
            | Self::Expand { .. } => 3600,
            // Kolla pulls images and runs the compute play on the new host.
            Self::DeployCompute { .. } => 7200,
            Self::SshKeys { .. } => 300,
            // Each moves a whole VM disk.
            Self::ExportSnapshot { .. } | Self::ImportSnapshot { .. } => 7200,
            Self::RemoveSnapshot { .. } | Self::FindSnapshot { .. } => 300,
            // A whole template disk crosses the network, then is loaded.
            Self::FetchArchive { .. }
            | Self::ReceiveArchive { .. }
            | Self::ImportArchive { .. }
            | Self::ExportArchive { .. }
            | Self::SendArchive { .. } => 7200,
            Self::VerifyVm => 300,
            // Downloads the release bundle; the script itself stops after 600 s.
            Self::InstallMsb => 660,
            _ => 60,
        }
    }
}
/// The part of a stackbox that spans hosts which one host runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Share {
    /// Suffixes of the stackbox's VMs on this host (`control`, `compute-1`, ...).
    pub nodes: Vec<String>,
    /// This host's switch joins the stackbox's segments on the other hosts through it.
    pub uplink: crate::link::UplinkConfig,
    /// Key Kolla on the control VM reaches the computes with, in OpenSSH format; only the
    /// control VM's host gets the private half.
    pub ssh_public_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_private_key: Option<String>,
}
impl Share {
    /// Check the share against the stackbox's plan.
    pub fn validate(&self, stackbox_id: &str, plan: &Plan) -> anyhow::Result<()> {
        use anyhow::ensure;
        let suffixes: Vec<String> = plan
            .nodes
            .iter()
            .filter_map(|spec| crate::stackbox::suffix(spec.role, spec.index))
            .collect();
        ensure!(plan.computes > 0, "only multi-node stackboxes span hosts");
        ensure!(
            !self.nodes.is_empty()
                && self.nodes.len() < suffixes.len()
                && self.nodes.iter().all(|n| suffixes.contains(n)),
            "the share names VMs the stackbox does not have"
        );
        let control = self.nodes.iter().any(|n| n == "control");
        ensure!(
            control == self.ssh_private_key.is_some(),
            "only the control VM's host gets the SSH private key"
        );
        ensure!(
            self.ssh_public_key.starts_with("ssh-ed25519 ")
                && !self.ssh_public_key.contains(['\'', '\n']),
            "invalid SSH public key"
        );
        let mut segments: Vec<String> = crate::stackbox::NETWORKS
            .iter()
            .map(|(_, network, _)| format!("{stackbox_id}/{network}"))
            .collect();
        let mut carried = self.uplink.segments.clone();
        segments.sort();
        carried.sort();
        ensure!(
            carried == segments,
            "the uplink carries other segments than the stackbox's"
        );
        // An empty CA trusts the public ones, for a relay behind a TLS proxy.
        if !self.uplink.ca.is_empty() {
            crate::link::tls::validate_ca(&self.uplink.ca)?;
        }
        ensure!(self.uplink.url.starts_with("wss://"), "uplinks use TLS");
        ensure!(
            self.uplink.key.len() == 64 && self.uplink.key.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid uplink key"
        );
        ensure!(
            self.uplink.token.len() == 64
                && self.uplink.token.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid uplink token"
        );
        Ok(())
    }
    pub fn has_control(&self) -> bool {
        self.nodes.iter().any(|n| n == "control")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    Phase {
        phase: Phase,
    },
    /// A finer step inside the phase, forwarded to the coordinator's stackbox record.
    Step {
        step: String,
    },
    /// A numbered step of an operation whose steps are known up front, such as a freeze.
    /// Coordinators older than it reject it, but they also send no freeze to a newer agent.
    Progress {
        step: String,
        index: u32,
        total: u32,
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
        /// Takes `FindSnapshot`, `FetchArchive`, `ReceiveArchive` and `ImportArchive`, so
        /// templates can be copied to this host. Agents built since also take
        /// `ExportArchive` and `SendArchive`, which move a snapshot frozen there elsewhere.
        #[serde(default)]
        supports_template_transfer: bool,
    },
    Inventory {
        stackboxes: BTreeMap<String, Vec<StackboxNode>>,
    },
    PublicPorts {
        bases: Vec<u16>,
    },
    Nodes {
        nodes: Vec<StackboxNode>,
    },
    Endpoints {
        endpoints: Endpoints,
    },
    Frozen {
        snapshot: FrozenSnapshot,
    },
    /// The host as an inspection reports it: the answer to `Inspect`, and to `VerifyVm`, whose
    /// verification boot updates `mtty_available` and `gpu_kernel`. Agents built before
    /// `VerifyVm` answered with this reply send `Done`; the coordinator then re-inspects and
    /// has no verification evidence. Boxed because an inspection is far larger than any other
    /// reply, and every frame on the wire carries one reply.
    Inspection {
        inspection: Box<super::HostInspection>,
    },
    Imported {
        snapshot: ImportedSnapshot,
    },
    /// The answer to `ExportArchive`: what `SendArchive` will stream.
    Archived {
        sha256: String,
        size_bytes: u64,
    },
    /// The answer to `FindSnapshot`: the msb reference of the cached copy, if any.
    /// For a multi-node snapshot, `nodes` holds every VM's reference on this host.
    Found {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        nodes: Vec<crate::stackbox::NodeSnapshot>,
    },
    SshKeys {
        public_key: String,
        private_key: String,
    },
    Done,
    Purged {
        /// Stackboxes whose VMs were destroyed, including ones already deleted from the registry.
        stackboxes: usize,
        /// Stackboxes of other coordinators still reserved on the host after a `PurgeOwn`. None when
        /// the reply does not say (an agent that never reported it), so the caller keeps the shared files.
        #[serde(default)]
        kept_for_others: Option<usize>,
    },
    GoldenRemoved {
        /// Golden snapshot groups deleted from the host.
        groups: usize,
    },
}

impl Operation {
    /// Whether the coordinator keeps writing `TerminalFrame` lines after the request.
    pub fn streams_input(&self) -> bool {
        matches!(
            self,
            Self::Shell { .. } | Self::ReceiveArchive { .. } | Self::SendArchive { .. }
        )
    }
    /// Whether the agent reports phases and steps while running this operation.
    fn reports_progress(&self) -> bool {
        matches!(
            self,
            Self::Deploy { .. }
                | Self::Attach { .. }
                | Self::Settle { .. }
                | Self::ForgetComputes { .. }
                | Self::AttachPci { .. }
                | Self::DeployCompute { .. }
        )
    }
    /// Whether the agent may report numbered steps while running this operation.
    fn reports_steps(&self) -> bool {
        matches!(self, Self::Freeze { .. })
    }
    /// Whether the agent may stream command output while running this operation.
    fn reports_output(&self) -> bool {
        !matches!(
            self,
            Self::Endpoints { .. }
                | Self::Hello
                | Self::Scan
                | Self::PublicPorts
                | Self::Inspect
                | Self::PciBind { .. }
                | Self::PciUnbind { .. }
        )
    }
    /// The helper verb and the device of a PCI operation: `bind` or `unbind`.
    pub fn pci_request(&self) -> Option<(&'static str, &str)> {
        match self {
            Self::PciBind { bdf } => Some(("bind", bdf)),
            Self::PciUnbind { bdf } => Some(("unbind", bdf)),
            _ => None,
        }
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
            anyhow::bail!("unexpected stackbox-agent terminal event")
        }
        Frame::Phase { phase } if operation.reports_progress() => DeployEvent::Phase(phase),
        Frame::Step { step } if operation.reports_progress() => DeployEvent::Step(step),
        Frame::Progress { step, index, total } if operation.reports_steps() => {
            DeployEvent::Progress(crate::kolla::StepProgress { step, index, total })
        }
        Frame::Output { message } if operation.reports_output() => DeployEvent::Output(message),
        _ => anyhow::bail!("unexpected stackbox-agent progress event"),
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
    /// The coordinator is done writing: EOF for the shell's stdin, while the session stays
    /// open until the shell exits. Closing the channel instead means the client left.
    Eof,
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
        let purged: Reply = serde_json::from_str(r#"{"result":"purged","stackboxes":3}"#).unwrap();
        assert!(matches!(
            purged,
            Reply::Purged {
                stackboxes: 3,
                kept_for_others: None
            }
        ));
        let counted: Reply =
            serde_json::from_str(r#"{"result":"purged","stackboxes":0,"kept_for_others":0}"#)
                .unwrap();
        assert!(matches!(
            counted,
            Reply::Purged {
                kept_for_others: Some(0),
                ..
            }
        ));
    }

    #[test]
    fn freeze_reports_numbered_steps_and_others_do_not() {
        use std::sync::{Arc, Mutex};
        let frame: Frame = serde_json::from_str(
            r#"{"type":"progress","step":"capture stackbox-a-control (1/2)","index":3,"total":7}"#,
        )
        .unwrap();
        let freeze = Operation::Freeze {
            stackbox_id: "a".into(),
            generation: "g".into(),
            snapshot_id: "s".into(),
            node: None,
            labels: BTreeMap::new(),
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let sink: crate::kolla::EventSink = Arc::new(move |event| {
            if let crate::kolla::DeployEvent::Progress(progress) = event {
                record.lock().unwrap().push(progress);
            }
            Ok(())
        });
        assert!(
            deliver(frame.clone(), &freeze, Some(&sink))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            seen.lock().unwrap()[0],
            crate::kolla::StepProgress {
                step: "capture stackbox-a-control (1/2)".into(),
                index: 3,
                total: 7,
            }
        );
        let delete = Operation::Delete {
            stackbox_id: "a".into(),
            generation: "g".into(),
        };
        assert!(deliver(frame, &delete, Some(&sink)).is_err());
    }

    #[test]
    fn a_deploy_request_from_before_the_resolved_ref_still_parses() {
        // An older coordinator sends no ref, and the agent deploys as it did before.
        let legacy: Request = serde_json::from_str(
            r#"{"version":1,"coordinator_id":"c","operation":{"operation":"deploy","stackbox_id":"f","generation":"g","from":"install","job_id":"j"}}"#,
        )
        .unwrap();
        assert!(matches!(
            legacy.operation,
            Operation::Deploy {
                reference: None,
                ..
            }
        ));
        // A new coordinator's request carries it, and the agent reads the same value back.
        let request = Request {
            version: VERSION,
            coordinator_id: "c".into(),
            operation: Operation::Deploy {
                stackbox_id: "f".into(),
                generation: "g".into(),
                from: Phase::Install,
                job_id: "j".into(),
                reference: Some("unmaintained/2025.1".into()),
            },
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(encoded.contains(r#""reference":"unmaintained/2025.1""#));
        let decoded: Request = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(
            decoded.operation,
            Operation::Deploy {
                reference: Some(reference),
                ..
            } if reference == "unmaintained/2025.1"
        ));
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
