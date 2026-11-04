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
    pub async fn deploy_job(
        &self,
        id: String,
        from: Phase,
        job_id: String,
        sink: EventSink,
    ) -> Result<()> {
        self.done(
            Operation::Deploy {
                farm_id: id,
                generation: self.generation.clone(),
                from,
                job_id,
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
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.done(
            Operation::Prepare {
                farm_id: id,
                generation: self.generation.clone(),
            },
            None,
        ))
    }
    fn deploy(
        &self,
        id: String,
        from: Phase,
        _: Option<Exposure>,
        // The agent deploys the components recorded with its own reservation.
        _: Vec<String>,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.deploy_job(id, from, uuid::Uuid::new_v4().to_string(), sink))
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
    fn freeze(
        &self,
        id: String,
        snapshot_id: String,
        node: Option<String>,
        labels: BTreeMap<String, String>,
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
                    None,
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
