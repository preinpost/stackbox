use crate::link::{Impair, host::Switch};
use crate::{
    farm::{self, FarmNode, FrozenSnapshot, ImportedSnapshot, TerminalIo, TerminalSize},
    kolla::{self, EventSink, Phase},
    model::{Datastore, Endpoints, Exposure, Plan},
};
use anyhow::{Result, anyhow};
use futures::future::BoxFuture;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

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
    /// Add faults to the links of a multi-node farm's VM (`control`, `compute-1`, ...);
    /// `Impair::default()` heals them.
    fn impair(&self, _id: String, _node: String, _impair: Impair) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Err(anyhow!("this backend has no links")) })
    }
    /// Run an interactive login shell in the farm's VM until it exits or `io.input` closes.
    fn terminal(
        &self,
        _id: String,
        _size: TerminalSize,
        _io: TerminalIo,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Err(anyhow!("this backend has no terminal")) })
    }
}
/// The farms of the host its agent directory belongs to, whose link switch it runs.
pub struct MicrosandboxBackend {
    switch: Switch,
}
impl MicrosandboxBackend {
    pub fn new(agent_dir: &Path) -> Self {
        Self {
            switch: Switch::new(agent_dir),
        }
    }
}
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
            farm::create_farm_with(
                &id,
                &plan.nodes,
                Some(&exposure),
                &plan.components,
                Some(&self.switch),
            )
            .await
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
        Box::pin(async move {
            farm::delete_farm(&id).await?;
            self.switch.remove_ports(&id)?;
            Ok(())
        })
    }
    fn impair(&self, id: String, node: String, impair: Impair) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            for (_, network, _) in farm::NETWORKS {
                self.switch
                    .impair(&format!("{id}/{node}/{network}"), impair)?;
            }
            Ok(())
        })
    }
    fn endpoints(
        &self,
        id: String,
        exposure: Option<Exposure>,
        components: Vec<String>,
    ) -> BoxFuture<'_, Result<Endpoints>> {
        Box::pin(async move {
            // New farms keep Kolla's configuration on the all-in-one or control VM. Legacy
            // farms remain readable/deletable; their credentials live on the old deployer.
            let name = match farm::all_in_one_name(&id).await {
                Ok(name) => name,
                Err(error) => farm::get_farm(&id)
                    .await?
                    .into_iter()
                    .find(|node| node.role == Some(farm::Role::Deployer))
                    .map(|node| node.name)
                    .ok_or(error)?,
            };
            let sandbox = farm::connect(&name).await?;
            let result = crate::shell::run(&sandbox, r#"set -euo pipefail
. /etc/kolla/admin-openrc.sh
python3 - <<'PY'
import os, json
keys = ['OS_AUTH_URL', 'OS_USERNAME', 'OS_PASSWORD', 'OS_PROJECT_NAME', 'OS_USER_DOMAIN_NAME', 'OS_PROJECT_DOMAIN_NAME', 'OS_REGION_NAME', 'OS_IDENTITY_API_VERSION', 'OS_INTERFACE']
print(json.dumps({k: os.environ[k] for k in keys if k in os.environ}))
PY"#).await;
            // The data stores report their own credentials and live schemas.
            let stores = crate::shell::run(&sandbox, DATASTORES_SCRIPT).await;
            sandbox.detach().await;
            let mut credentials: BTreeMap<String, String> = serde_json::from_str(&result?)?;
            let datastores = exposure
                .as_ref()
                .map(|e| datastores(&e.clone(), stores.unwrap_or_default()))
                .unwrap_or_default();
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
                datastores,
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
    fn terminal(
        &self,
        id: String,
        size: TerminalSize,
        io: TerminalIo,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { farm::attach_terminal(&id, size, io).await.map(|_| ()) })
    }
}
/// Reads the data store credentials from Kolla's generated passwords and the live schemas.
const DATASTORES_SCRIPT: &str = r#"set -euo pipefail
python3 - <<'PY'
import json, re, subprocess
passwords = open('/etc/kolla/passwords.yml').read()
def value(key):
    match = re.search(rf'^{key}:\s*(\S+)\s*$', passwords, re.MULTILINE)
    return match.group(1) if match else None
databases = []
try:
    out = subprocess.run(['docker', 'exec', 'mariadb', 'mariadb', '-uroot',
                          f"-p{value('database_password') or ''}", '-N', '-e', 'show databases'],
                         capture_output=True, text=True, timeout=60)
    own = {'information_schema', 'performance_schema', 'mysql', 'sys'}
    databases = [line.strip() for line in out.stdout.splitlines() if line.strip() not in own]
except Exception:
    pass
print(json.dumps({
    'database_password': value('database_password'),
    'rabbitmq_password': value('rabbitmq_password'),
    'rabbitmq_user': value('rabbitmq_userid') or 'openstack',
    'databases': databases,
}))
PY"#;

/// Builds the data store list from the guest's own report, keyed by published port.
fn datastores(exposure: &Exposure, report: String) -> Vec<Datastore> {
    let report: serde_json::Value = serde_json::from_str(report.trim()).unwrap_or_default();
    let field = |key: &str| {
        report
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let databases: Vec<String> = report
        .get("databases")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    crate::model::INFRA_PORTS
        .iter()
        .map(|port| {
            let host = exposure.host.clone();
            let host_port = exposure.port_base + port.offset;
            let mut store = Datastore {
                id: port.id.into(),
                label: port.label.into(),
                protocol: "http".into(),
                host: host.clone(),
                port: host_port,
                username: None,
                password: None,
                vhost: None,
                url: format!("http://{host}:{host_port}/"),
                databases: vec![],
            };
            match port.id {
                "mariadb" => {
                    store.protocol = "mysql".into();
                    store.username = Some("root".into());
                    store.password = field("database_password");
                    store.databases = databases.clone();
                    store.url = format!("mysql://root@{host}:{host_port}/");
                }
                "rabbitmq" => {
                    store.protocol = "amqp".into();
                    store.vhost = Some("/".into());
                    store.username =
                        Some(field("rabbitmq_user").unwrap_or_else(|| "openstack".into()));
                    store.password = field("rabbitmq_password");
                    store.url = format!("amqp://{host}:{host_port}/%2f");
                }
                // The management UI signs in with the same RabbitMQ account.
                _ => {
                    store.username =
                        Some(field("rabbitmq_user").unwrap_or_else(|| "openstack".into()));
                    store.password = field("rabbitmq_password");
                }
            }
            store
        })
        .collect()
}
