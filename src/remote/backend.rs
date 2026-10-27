use super::{
    link::AgentLink,
    protocol::{Operation, Reply, Request, VERSION},
};
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
        }))
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
        Box::pin(self.done(
            Operation::Deploy {
                farm_id: id,
                generation: self.generation.clone(),
                from,
            },
            Some(sink),
        ))
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
        labels: BTreeMap<String, String>,
    ) -> BoxFuture<'_, Result<FrozenSnapshot>> {
        Box::pin(async move {
            match self
                .call(
                    Operation::Freeze {
                        farm_id: id,
                        generation: self.generation.clone(),
                        snapshot_id,
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
        plan: Plan,
        exposure: Exposure,
    ) -> BoxFuture<'_, Result<Vec<FarmNode>>> {
        Box::pin(self.nodes(Operation::Restore {
            farm_id: id,
            generation: self.generation.clone(),
            reference,
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
    fn remove_snapshot(&self, reference: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.done(Operation::RemoveSnapshot { reference }, None))
    }
    fn terminal(
        &self,
        id: String,
        size: TerminalSize,
        io: TerminalIo,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.link.terminal(
            self.request(Operation::Shell {
                farm_id: id,
                generation: self.generation.clone(),
                size,
            }),
            io,
        ))
    }
}
