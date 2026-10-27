use super::{
    Credentials, RemoteHost,
    protocol::{Operation, Reply, Request, VERSION},
    ssh::Session,
};
use crate::{
    farm::{FarmNode, TerminalIo, TerminalSize},
    kolla::{EventSink, Phase},
    manager::Backend,
    model::{Endpoints, Exposure, Plan},
};
use anyhow::{Result, bail};
use futures::future::BoxFuture;
use std::collections::BTreeMap;

pub struct RemoteBackend {
    pub host: RemoteHost,
    pub credentials: Credentials,
    pub sha256: String,
    pub coordinator_id: String,
    pub generation: String,
}
impl RemoteBackend {
    async fn call(&self, operation: Operation, sink: Option<EventSink>) -> Result<Reply> {
        let session = Session::connect(&self.host, self.credentials.clone()).await?;
        session
            .rpc(
                &self.sha256,
                &Request {
                    version: VERSION,
                    coordinator_id: self.coordinator_id.clone(),
                    operation,
                },
                sink,
            )
            .await
    }
}
impl Backend for RemoteBackend {
    fn scan(&self) -> BoxFuture<'_, Result<BTreeMap<String, Vec<FarmNode>>>> {
        Box::pin(async {
            match self.call(Operation::Scan, None).await? {
                Reply::Inventory { farms } => Ok(farms),
                _ => bail!("unexpected remote inventory response"),
            }
        })
    }
    fn create(
        &self,
        id: String,
        plan: Plan,
        exposure: Exposure,
    ) -> BoxFuture<'_, Result<Vec<FarmNode>>> {
        Box::pin(async move {
            match self
                .call(
                    Operation::Create {
                        farm_id: id,
                        generation: self.generation.clone(),
                        plan,
                        exposure,
                    },
                    None,
                )
                .await?
            {
                Reply::Nodes { nodes } => Ok(nodes),
                _ => bail!("unexpected remote create response"),
            }
        })
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
        Box::pin(async move {
            match self
                .call(
                    Operation::Deploy {
                        farm_id: id,
                        generation: self.generation.clone(),
                        from,
                    },
                    Some(sink),
                )
                .await?
            {
                Reply::Done => Ok(()),
                _ => bail!("unexpected remote deploy response"),
            }
        })
    }
    fn delete(&self, id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            match self
                .call(
                    Operation::Delete {
                        farm_id: id,
                        generation: self.generation.clone(),
                    },
                    None,
                )
                .await?
            {
                Reply::Done => Ok(()),
                _ => bail!("unexpected remote delete response"),
            }
        })
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
                _ => bail!("unexpected remote endpoints response"),
            }
        })
    }
    fn terminal(
        &self,
        id: String,
        size: TerminalSize,
        io: TerminalIo,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let session = Session::connect(&self.host, self.credentials.clone()).await?;
            session
                .terminal(
                    &self.sha256,
                    &Request {
                        version: VERSION,
                        coordinator_id: self.coordinator_id.clone(),
                        operation: Operation::Shell {
                            farm_id: id,
                            generation: self.generation.clone(),
                            size,
                        },
                    },
                    io,
                )
                .await
        })
    }
}
