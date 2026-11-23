use super::{
    link::AgentLink,
    protocol::{Operation, Reply, Request, VERSION},
};
use crate::link::Impair;
use crate::{
    farm::{FarmNode, FrozenSnapshot, ImportedSnapshot, TerminalIo, TerminalSize},
    kolla::{EventSink, Phase},
    manager::Backend,
    model::{Endpoints, Exposure, Plan},
};
use anyhow::{Result, bail};
use futures::future::BoxFuture;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

/// Drives one farm generation on a host through that host's farm-agent.
pub struct AgentBackend {
    pub link: Arc<dyn AgentLink>,
    pub coordinator_id: String,
    pub generation: String,
}
impl AgentBackend {
    fn request(&self, operation: Operation) -> Request {
        Request {
            version: VERSION,
            coordinator_id: self.coordinator_id.clone(),
            operation,
        }
    }
    async fn call(&self, operation: Operation, sink: Option<EventSink>) -> Result<Reply> {
        self.link.call(self.request(operation), sink).await
    }
    async fn done(&self, operation: Operation, sink: Option<EventSink>) -> Result<()> {
        match self.call(operation, sink).await? {
            Reply::Done => Ok(()),
            _ => bail!("unexpected farm-agent response"),
        }
    }
    /// Deploy as the host-side job `job_id`, which keeps running if this request ends.
    /// `reference` is the kolla-ansible ref the coordinator resolved for the farm's release;
    /// the agent has no catalog cache to resolve it itself.
    pub async fn deploy_job(
        &self,
        id: String,
        from: Phase,
        job_id: String,
        reference: Option<String>,
        sink: EventSink,
    ) -> Result<()> {
        self.done(
            Operation::Deploy {
                farm_id: id,
                generation: self.generation.clone(),
                from,
                job_id,
                reference,
            },
            Some(sink),
        )
        .await
    }
    /// Follow the host-side job `job_id` from its `offset`th progress frame to its result.
    pub async fn attach(&self, job_id: String, offset: u64, sink: EventSink) -> Result<()> {
        self.done(Operation::Attach { job_id, offset }, Some(sink))
            .await
    }
    /// Whether the host's agent takes the template transfer operations.
    pub async fn supports_template_transfer(&self) -> Result<bool> {
        match self.call(Operation::Hello, None).await? {
            Reply::Hello {
                supports_template_transfer,
                ..
            } => Ok(supports_template_transfer),
            _ => bail!("unexpected farm-agent hello response"),
        }
    }
    /// The reference of the copy of `snapshot_id` this coordinator left on the host.
    pub async fn find_snapshot(
        &self,
        snapshot_id: String,
    ) -> Result<Option<(String, Vec<crate::farm::NodeSnapshot>)>> {
        match self
            .call(Operation::FindSnapshot { snapshot_id }, None)
            .await?
        {
            Reply::Found {
                reference: Some(reference),
                nodes,
            } => Ok(Some((reference, nodes))),
            Reply::Found {
                reference: None, ..
            } => Ok(None),
            _ => bail!("unexpected farm-agent response"),
        }
    }
    /// Have the host download an archive from a presigned URL into transfer `transfer_id`.
    pub async fn fetch_archive(
        &self,
        transfer_id: String,
        url: String,
        ca_pem: Option<String>,
        sha256: String,
        size_bytes: u64,
        sink: EventSink,
    ) -> Result<()> {
        self.done(
            Operation::FetchArchive {
                transfer_id,
                url,
                ca_pem,
                sha256,
                size_bytes,
            },
            Some(sink),
        )
        .await
    }
    /// Stream the archive at `path` to the host as transfer `transfer_id`; `progress` gets
    /// the bytes sent so far.
    pub async fn send_archive(
        &self,
        transfer_id: String,
        path: PathBuf,
        sha256: String,
        size_bytes: u64,
        mut progress: impl FnMut(u64) + Send,
    ) -> Result<()> {
        use tokio::io::AsyncReadExt;
        // Base64 in a JSON line must stay under the frame limit.
        const CHUNK: usize = 96 * 1024;
        let (input, input_rx) = tokio::sync::mpsc::channel(8);
        let (output, mut output_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let session = self.link.terminal(
            self.request(Operation::ReceiveArchive {
                transfer_id,
                sha256,
                size_bytes,
            }),
            crate::farm::TerminalIo {
                input: input_rx,
                output,
            },
        );
        let feed = async move {
            let mut file = tokio::fs::File::open(&path).await?;
            let mut sent = 0;
            loop {
                let mut chunk = vec![0; CHUNK];
                let read = file.read(&mut chunk).await?;
                if read == 0 {
                    break;
                }
                chunk.truncate(read);
                if input
                    .send(crate::farm::TerminalInput::Data(chunk))
                    .await
                    .is_err()
                {
                    // The session ended early; its result says why.
                    break;
                }
                sent += read as u64;
                progress(sent);
            }
            // Dropping `input` ends the stream.
            Ok::<_, anyhow::Error>(())
        };
        let drain = async { while output_rx.recv().await.is_some() {} };
        let (session, fed, ()) = tokio::join!(session, feed, drain);
        session?;
        fed
    }
    /// Export the snapshot at `reference` into transfer `transfer_id` on the host, for
    /// [`Self::receive_archive`]. Returns its SHA-256 and size.
    pub async fn export_archive(
        &self,
        transfer_id: String,
        reference: String,
    ) -> Result<(String, u64)> {
        match self
            .call(
                Operation::ExportArchive {
                    transfer_id,
                    reference,
                },
                None,
            )
            .await?
        {
            Reply::Archived { sha256, size_bytes } => Ok((sha256, size_bytes)),
            _ => bail!("unexpected farm-agent export response"),
        }
    }
    /// Stream transfer `transfer_id`'s exported archive from the host into `path`, which
    /// must end up `size_bytes` long with SHA-256 `sha256`; `progress` gets the bytes
    /// received so far.
    pub async fn receive_archive(
        &self,
        transfer_id: String,
        path: PathBuf,
        (sha256, size_bytes): (String, u64),
        mut progress: impl FnMut(u64) + Send,
    ) -> Result<()> {
        use sha2::{Digest, Sha256};
        use tokio::io::AsyncWriteExt;
        // Held open until the session ends: some links hang up soon after the input closes.
        let (_input, input_rx) = tokio::sync::mpsc::channel(1);
        let (output, mut output_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let session = self.link.terminal(
            self.request(Operation::SendArchive { transfer_id }),
            crate::farm::TerminalIo {
                input: input_rx,
                output,
            },
        );
        let write = async {
            let mut file = tokio::fs::File::create(&path).await?;
            let mut hash = Sha256::new();
            let mut received = 0;
            while let Some(data) = output_rx.recv().await {
                received += data.len() as u64;
                if received > size_bytes {
                    bail!("the archive is longer than announced");
                }
                hash.update(&data);
                file.write_all(&data).await?;
                progress(received);
            }
            file.sync_all().await?;
            Ok::<_, anyhow::Error>((received, format!("{:x}", hash.finalize())))
        };
        let (session, written) = tokio::join!(session, write);
        let written = session.and(written);
        let (received, digest) = match written {
            Ok(written) => written,
            Err(error) => {
                let _ = tokio::fs::remove_file(&path).await;
                return Err(error);
            }
        };
        if received != size_bytes || !digest.eq_ignore_ascii_case(&sha256) {
            let _ = tokio::fs::remove_file(&path).await;
            bail!(
                "received {received} bytes with SHA-256 {digest}, but the host exported {size_bytes} bytes with SHA-256 {sha256}"
            );
        }
        Ok(())
    }
    /// Load transfer `transfer_id`'s verified archive on the host as `snapshot_id`.
    pub async fn import_archive(
        &self,
        transfer_id: String,
        snapshot_id: String,
    ) -> Result<ImportedSnapshot> {
        match self
            .call(
                Operation::ImportArchive {
                    transfer_id,
                    snapshot_id,
                },
                None,
            )
            .await?
        {
            Reply::Imported { snapshot } => Ok(snapshot),
            _ => bail!("unexpected farm-agent import response"),
        }
    }
    async fn nodes(&self, operation: Operation) -> Result<Vec<FarmNode>> {
        match self.call(operation, None).await? {
            Reply::Nodes { nodes } => Ok(nodes),
            _ => bail!("unexpected farm-agent response"),
        }
    }
}
impl Backend for AgentBackend {
    fn scan(&self) -> BoxFuture<'_, Result<BTreeMap<String, Vec<FarmNode>>>> {
        Box::pin(async {
            match self.call(Operation::Scan, None).await? {
                Reply::Inventory { farms } => Ok(farms),
                _ => bail!("unexpected farm-agent inventory response"),
            }
        })
    }
    fn public_ports(&self) -> BoxFuture<'_, Result<Vec<u16>>> {
        Box::pin(async {
            match self.call(Operation::PublicPorts, None).await? {
                Reply::PublicPorts { bases } => Ok(bases),
                _ => bail!("unexpected farm-agent public ports response"),
            }
        })
    }
    fn create(
        &self,
        id: String,
        plan: Plan,
        exposure: Exposure,
    ) -> BoxFuture<'_, Result<Vec<FarmNode>>> {
        Box::pin(self.nodes(Operation::Create {
            farm_id: id,
            generation: self.generation.clone(),
            plan,
            exposure,
            share: None,
            source: None,
        }))
    }
    fn create_share(
        &self,
        id: String,
        plan: Plan,
        exposure: Exposure,
        share: super::protocol::Share,
    ) -> BoxFuture<'_, Result<Vec<FarmNode>>> {
        Box::pin(self.nodes(Operation::Create {
            farm_id: id,
            generation: self.generation.clone(),
            plan,
            exposure,
            share: Some(Box::new(share)),
            source: None,
        }))
    }
    fn create_share_from(
        &self,
        id: String,
        plan: Plan,
        exposure: Exposure,
        share: super::protocol::Share,
        source: String,
    ) -> BoxFuture<'_, Result<Vec<FarmNode>>> {
        Box::pin(self.nodes(Operation::Create {
            farm_id: id,
            generation: self.generation.clone(),
            plan,
            exposure,
            share: Some(Box::new(share)),
            source: Some(source),
        }))
    }
    fn expand(
        &self,
        id: String,
        plan: Plan,
        nodes: Vec<String>,
        add: Vec<String>,
        uplink: Option<crate::link::UplinkConfig>,
        source: Option<String>,
    ) -> BoxFuture<'_, Result<Vec<FarmNode>>> {
        Box::pin(self.nodes(Operation::Expand {
            farm_id: id,
            generation: self.generation.clone(),
            plan,
            nodes,
            add,
            uplink,
            source,
        }))
    }
    fn ssh_keys(&self, id: String) -> BoxFuture<'_, Result<(String, String)>> {
        Box::pin(async move {
            match self
                .call(
                    Operation::SshKeys {
                        farm_id: id,
                        generation: self.generation.clone(),
                    },
                    None,
                )
                .await?
            {
                Reply::SshKeys {
                    public_key,
                    private_key,
                } => Ok((public_key, private_key)),
                _ => bail!("unexpected farm-agent response"),
            }
        })
    }
    fn deploy_compute(
        &self,
        id: String,
        _plan: Plan,
        index: u8,
        _nodes: Vec<String>,
        _components: Vec<String>,
        from_template: bool,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.done(
            Operation::DeployCompute {
                farm_id: id,
                generation: self.generation.clone(),
                index,
                from_template,
            },
            Some(sink),
        ))
    }
    fn relink(&self, id: String, url: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.done(
            Operation::Relink {
                farm_id: id,
                generation: self.generation.clone(),
                url,
            },
            None,
        ))
    }
    fn prepare(
        &self,
        id: String,
        // The agent prepares with what it recorded at create.
        _: Vec<String>,
        _: String,
        _: String,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.done(
            Operation::Prepare {
                farm_id: id,
                generation: self.generation.clone(),
            },
            None,
        ))
    }
    #[allow(clippy::too_many_arguments)]
    fn deploy(
        &self,
        id: String,
        from: Phase,
        _: Option<Exposure>,
        // The agent deploys the components, release and GPU recorded with its own reservation.
        _: Vec<String>,
        _: String,
        reference: Option<String>,
        _: Option<crate::model::GpuConfig>,
        // The agent deploys the PCI devices recorded with its own reservation, too.
        _: Vec<String>,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.deploy_job(id, from, uuid::Uuid::new_v4().to_string(), reference, sink))
    }
    fn delete(&self, id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.done(
            Operation::Delete {
                farm_id: id,
                generation: self.generation.clone(),
            },
            None,
        ))
    }
    fn endpoints(
        &self,
        id: String,
        _: Option<Exposure>,
        _: Vec<String>,
    ) -> BoxFuture<'_, Result<Endpoints>> {
        Box::pin(async move {
            match self
                .call(
                    Operation::Endpoints {
                        farm_id: id,
                        generation: self.generation.clone(),
                    },
                    None,
                )
                .await?
            {
                Reply::Endpoints { endpoints } => Ok(endpoints),
                _ => bail!("unexpected farm-agent endpoints response"),
            }
        })
    }
    fn control_script(&self, id: String, script: String) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            // The Shell operation already opens a login shell in the farm's VM; farm drives
            // it as a non-interactive `bash -s` session, so no protocol version changes.
            let request = self.request(Operation::Shell {
                farm_id: id,
                generation: self.generation.clone(),
                node: None,
                size: crate::farm::TerminalSize {
                    rows: 40,
                    cols: 240,
                },
            });
            let link = self.link.clone();
            crate::farm::ScriptSession::new(move |io| {
                Box::pin(async move { link.terminal(request, io).await })
            })
            .run(&script)
            .await
        })
    }
    fn freeze(
        &self,
        id: String,
        snapshot_id: String,
        node: Option<String>,
        labels: BTreeMap<String, String>,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<FrozenSnapshot>> {
        Box::pin(async move {
            match self
                .call(
                    Operation::Freeze {
                        farm_id: id,
                        generation: self.generation.clone(),
                        snapshot_id,
                        node,
                        labels,
                    },
                    Some(sink),
                )
                .await?
            {
                Reply::Frozen { snapshot } => Ok(snapshot),
                _ => bail!("unexpected farm-agent freeze response"),
            }
        })
    }
    fn restore(
        &self,
        id: String,
        reference: String,
        nodes: Vec<crate::farm::NodeSnapshot>,
        plan: Plan,
        exposure: Exposure,
    ) -> BoxFuture<'_, Result<Vec<FarmNode>>> {
        Box::pin(self.nodes(Operation::Restore {
            farm_id: id,
            generation: self.generation.clone(),
            reference,
            nodes,
            plan,
            exposure,
        }))
    }
    fn settle(
        &self,
        id: String,
        rebind: Option<(Exposure, Exposure)>,
        // The agent checks the components recorded with its own reservation.
        _: Vec<String>,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.done(
            Operation::Settle {
                farm_id: id,
                generation: self.generation.clone(),
                rebind,
            },
            Some(sink),
        ))
    }
    fn forget_computes(
        &self,
        id: String,
        // The agent uses the components recorded with its own reservation.
        _: Vec<String>,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.done(
            Operation::ForgetComputes {
                farm_id: id,
                generation: self.generation.clone(),
            },
            Some(sink),
        ))
    }
    fn export_snapshot(&self, reference: String, output: PathBuf) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.done(
            Operation::ExportSnapshot {
                reference,
                output: output.display().to_string(),
            },
            None,
        ))
    }
    fn import_snapshot(
        &self,
        archive: PathBuf,
        snapshot_id: String,
    ) -> BoxFuture<'_, Result<ImportedSnapshot>> {
        Box::pin(async move {
            match self
                .call(
                    Operation::ImportSnapshot {
                        archive: archive.display().to_string(),
                        snapshot_id,
                    },
                    None,
                )
                .await?
            {
                Reply::Imported { snapshot } => Ok(snapshot),
                _ => bail!("unexpected farm-agent import response"),
            }
        })
    }
    fn impair(&self, id: String, node: String, impair: Impair) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.done(
            Operation::Impair {
                farm_id: id,
                generation: self.generation.clone(),
                node,
                impair,
            },
            None,
        ))
    }
    fn remove_snapshot(&self, reference: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.done(Operation::RemoveSnapshot { reference }, None))
    }
    fn terminal(
        &self,
        id: String,
        node: Option<String>,
        size: TerminalSize,
        io: TerminalIo,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.link.terminal(
            self.request(Operation::Shell {
                farm_id: id,
                generation: self.generation.clone(),
                node,
                size,
            }),
            io,
        ))
    }
}
