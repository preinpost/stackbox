use crate::{
    farm::{self, FarmNode, FrozenSnapshot, ImportedSnapshot},
    kolla::{self, EventSink, Phase},
    model::{Endpoints, Exposure, Plan},
};
use anyhow::{Result, anyhow};
use futures::future::BoxFuture;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

fn unsupported<T: Send + 'static>() -> BoxFuture<'static, Result<T>> {
    Box::pin(async { Err(anyhow!("snapshots are available for local farms only")) })
}

/// VM boundary; tests can exercise the real manager without launching guests.
pub trait Backend: Send + Sync + 'static {
    fn scan(&self) -> BoxFuture<'_, Result<BTreeMap<String, Vec<FarmNode>>>>;
    fn create(
        &self,
        id: String,
        plan: Plan,
        exposure: Exposure,
    ) -> BoxFuture<'_, Result<Vec<FarmNode>>>;
    /// `components` is the farm plan's resolved service list.
    fn deploy(
        &self,
        id: String,
        from: Phase,
        exposure: Option<Exposure>,
        components: Vec<String>,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>>;
    fn delete(&self, id: String) -> BoxFuture<'_, Result<()>>;
    fn endpoints(
        &self,
        id: String,
        exposure: Option<Exposure>,
        components: Vec<String>,
    ) -> BoxFuture<'_, Result<Endpoints>>;
    /// Stop the farm's VM, capture its disks under `labels`, and start it again.
    fn freeze(
        &self,
        _id: String,
        _snapshot_id: String,
        _labels: BTreeMap<String, String>,
    ) -> BoxFuture<'_, Result<FrozenSnapshot>> {
        unsupported()
    }
    fn restore(
        &self,
        _id: String,
        _reference: String,
        _plan: Plan,
        _exposure: Exposure,
    ) -> BoxFuture<'_, Result<Vec<FarmNode>>> {
        unsupported()
    }
    /// Wait for a deployed farm's services after a cold start, optionally moving its
    /// public catalog entries from the first exposure to the second.
    fn settle(
        &self,
        _id: String,
        _rebind: Option<(Exposure, Exposure)>,
        _components: Vec<String>,
        _sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported()
    }
    fn export_snapshot(&self, _reference: String, _output: PathBuf) -> BoxFuture<'_, Result<()>> {
        unsupported()
    }
    fn import_snapshot(
        &self,
        _archive: PathBuf,
        _snapshot_id: String,
    ) -> BoxFuture<'_, Result<ImportedSnapshot>> {
        unsupported()
    }
    fn remove_snapshot(&self, _reference: String) -> BoxFuture<'_, Result<()>> {
        unsupported()
    }
}
pub struct MicrosandboxBackend;
impl Backend for MicrosandboxBackend {
    fn scan(&self) -> BoxFuture<'_, Result<BTreeMap<String, Vec<FarmNode>>>> {
        Box::pin(farm::scan_farms())
    }
    fn create(
        &self,
        id: String,
        plan: Plan,
        exposure: Exposure,
    ) -> BoxFuture<'_, Result<Vec<FarmNode>>> {
        Box::pin(async move {
            farm::create_farm_with(&id, &plan.nodes, Some(&exposure), &plan.components).await
        })
    }
    fn deploy(
        &self,
        id: String,
        from: Phase,
        exposure: Option<Exposure>,
        components: Vec<String>,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { kolla::deploy_with(&id, from, exposure, &components, sink).await })
    }
    fn delete(&self, id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { farm::delete_farm(&id).await.map(|_| ()) })
    }
    fn endpoints(
        &self,
        id: String,
        exposure: Option<Exposure>,
        components: Vec<String>,
    ) -> BoxFuture<'_, Result<Endpoints>> {
        Box::pin(async move {
            let nodes = farm::get_farm(&id).await?;
            // Legacy farms remain readable/deletable; their credentials still
            // live on the old deployer. New farms keep everything on one VM.
            let node = nodes
                .iter()
                .find(|node| node.role == Some(farm::Role::AllInOne))
                .or_else(|| {
                    nodes
                        .iter()
                        .find(|node| node.role == Some(farm::Role::Deployer))
                })
                .ok_or_else(|| anyhow::anyhow!("farm has no configuration VM"))?;
            let sandbox = farm::connect(&node.name).await?;
            let result = crate::shell::run(&sandbox, r#"set -euo pipefail
. /etc/kolla/admin-openrc.sh
python3 - <<'PY'
import os, json
keys = ['OS_AUTH_URL', 'OS_USERNAME', 'OS_PASSWORD', 'OS_PROJECT_NAME', 'OS_USER_DOMAIN_NAME', 'OS_PROJECT_DOMAIN_NAME', 'OS_REGION_NAME', 'OS_IDENTITY_API_VERSION', 'OS_INTERFACE']
print(json.dumps({k: os.environ[k] for k in keys if k in os.environ}))
PY"#).await;
            sandbox.detach().await;
            let mut credentials: BTreeMap<String, String> = serde_json::from_str(&result?)?;
            let services = exposure
                .map(|e| {
                    let urls = e.urls(Some(&components));
                    credentials.insert("OS_AUTH_URL".into(), urls["keystone"].clone());
                    credentials.insert("OS_INTERFACE".into(), "public".into());
                    urls
                })
                .unwrap_or_else(|| {
                    credentials
                        .get("OS_AUTH_URL")
                        .map(|url| BTreeMap::from([("keystone_internal".into(), url.clone())]))
                        .unwrap_or_default()
                });
            Ok(Endpoints {
                services,
                credentials,
            })
        })
    }
    fn freeze(
        &self,
        id: String,
        snapshot_id: String,
        labels: BTreeMap<String, String>,
    ) -> BoxFuture<'_, Result<FrozenSnapshot>> {
        Box::pin(async move { farm::freeze_farm(&id, &snapshot_id, &labels).await })
    }
    fn restore(
        &self,
        id: String,
        reference: String,
        plan: Plan,
        exposure: Exposure,
    ) -> BoxFuture<'_, Result<Vec<FarmNode>>> {
        Box::pin(async move {
            let [spec] = plan.nodes[..] else {
                anyhow::bail!("snapshots hold exactly one all-in-one VM");
            };
            farm::restore_farm(&id, &reference, spec, &exposure, &plan.components).await
        })
    }
    fn settle(
        &self,
        id: String,
        rebind: Option<(Exposure, Exposure)>,
        components: Vec<String>,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let rebind = rebind.as_ref().map(|(from, to)| (from, to));
            kolla::settle(&id, rebind, &components, sink).await
        })
    }
    fn export_snapshot(&self, reference: String, output: PathBuf) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { farm::export_snapshot(&reference, &output).await })
    }
    fn import_snapshot(
        &self,
        archive: PathBuf,
        snapshot_id: String,
    ) -> BoxFuture<'_, Result<ImportedSnapshot>> {
        Box::pin(async move { farm::import_snapshot(&archive, &snapshot_id).await })
    }
    fn remove_snapshot(&self, reference: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { farm::remove_snapshot(&reference).await })
    }
}
pub fn real() -> Arc<dyn Backend> {
    Arc::new(MicrosandboxBackend)
}
