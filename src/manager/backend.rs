use crate::link::{Impair, host::Switch};
use crate::{
    kolla::{self, EventSink, Phase},
    model::{Datastore, Endpoints, Exposure, Plan},
    stackbox::{self, FrozenSnapshot, ImportedSnapshot, StackboxNode, TerminalIo, TerminalSize},
};
use anyhow::{Context, Result, anyhow};
use futures::future::BoxFuture;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

fn unsupported<T: Send + 'static>() -> BoxFuture<'static, Result<T>> {
    Box::pin(async { Err(anyhow!("snapshots are available for local stackboxes only")) })
}

/// VM boundary; tests can exercise the real manager without launching guests.
pub trait Backend: Send + Sync + 'static {
    fn scan(&self) -> BoxFuture<'_, Result<BTreeMap<String, Vec<StackboxNode>>>>;
    /// Public port bases the host reserves for stackboxes of every coordinator.
    fn public_ports(&self) -> BoxFuture<'_, Result<Vec<u16>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn create(
        &self,
        id: String,
        plan: Plan,
        exposure: Exposure,
    ) -> BoxFuture<'_, Result<Vec<StackboxNode>>>;
    /// `components` and `release` are the stackbox plan's resolved services and OpenStack release;
    /// `reference` is the kolla-ansible ref the coordinator resolved for it.
    #[allow(clippy::too_many_arguments)]
    fn deploy(
        &self,
        id: String,
        from: Phase,
        exposure: Option<Exposure>,
        components: Vec<String>,
        release: String,
        reference: Option<String>,
        gpu: Option<crate::model::GpuConfig>,
        pci_devices: Vec<String>,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>>;
    fn delete(&self, id: String) -> BoxFuture<'_, Result<()>>;
    /// Delete the golden snapshot groups this host holds, under the golden build lock; the count.
    fn remove_golden(&self) -> BoxFuture<'_, Result<usize>>;
    /// Create this host's part of a stackbox that spans hosts.
    fn create_share(
        &self,
        _id: String,
        _plan: Plan,
        _exposure: Exposure,
        _share: crate::remote::protocol::Share,
    ) -> BoxFuture<'_, Result<Vec<StackboxNode>>> {
        Box::pin(async { Err(anyhow!("this backend runs stackboxes on one host only")) })
    }
    /// Create this host's part of a stackbox that spans hosts, booting its one compute from
    /// the compute template snapshot `source` on this host.
    fn create_share_from(
        &self,
        _id: String,
        _plan: Plan,
        _exposure: Exposure,
        _share: crate::remote::protocol::Share,
        _source: String,
    ) -> BoxFuture<'_, Result<Vec<StackboxNode>>> {
        Box::pin(async { Err(anyhow!("this backend cannot boot a share from a template")) })
    }
    /// Add compute VMs to an existing stackbox on this host. `nodes` are the stackbox's VMs this
    /// host runs after the change; `add` are the suffixes to create, from the compute
    /// template snapshot `source` if given. `uplink` joins this host's switch to the relay
    /// for the first time when a remote compute is added.
    fn expand(
        &self,
        _id: String,
        _plan: Plan,
        _nodes: Vec<String>,
        _add: Vec<String>,
        _uplink: Option<crate::link::UplinkConfig>,
        _source: Option<String>,
    ) -> BoxFuture<'_, Result<Vec<StackboxNode>>> {
        Box::pin(async { Err(anyhow!("this backend cannot add a compute VM")) })
    }
    /// The SSH key pair the control VM uses to reach the stackbox's computes.
    fn ssh_keys(&self, _id: String) -> BoxFuture<'_, Result<(String, String)>> {
        Box::pin(async { Err(anyhow!("this backend has no control VM SSH key")) })
    }
    /// Deploy one new compute VM with Kolla, limited to that VM. One booted from a compute
    /// template (`from_template`) is only reconfigured.
    #[allow(clippy::too_many_arguments)]
    fn deploy_compute(
        &self,
        _id: String,
        _plan: Plan,
        _index: u8,
        _nodes: Vec<String>,
        _components: Vec<String>,
        _from_template: bool,
        _sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Err(anyhow!("this backend cannot deploy a compute VM")) })
    }
    /// Point this host's switch at the relay's new `url` for a stackbox that spans hosts.
    fn relink(&self, _id: String, _url: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Err(anyhow!("this backend runs stackboxes on one host only")) })
    }
    /// Ready this host's compute VMs of a stackbox that spans hosts for Kolla: what the
    /// prepare phase does to computes, and Kolla's SSH key.
    fn prepare(
        &self,
        _id: String,
        _components: Vec<String>,
        _release: String,
        _ssh_public_key: String,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Err(anyhow!("this backend runs stackboxes on one host only")) })
    }
    /// Deploy a stackbox that spans hosts from its control VM's host. `remote` are the
    /// compute VMs on other hosts, which [`Backend::prepare`] readied there.
    #[allow(clippy::too_many_arguments)]
    fn deploy_spanning(
        &self,
        _id: String,
        _from: Phase,
        _exposure: Option<Exposure>,
        _components: Vec<String>,
        _release: String,
        _reference: Option<String>,
        _gpu: Option<crate::model::GpuConfig>,
        _pci_devices: Vec<String>,
        _remote: Vec<u8>,
        _ssh_private_key: String,
        _sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Err(anyhow!("this backend runs stackboxes on one host only")) })
    }
    fn endpoints(
        &self,
        id: String,
        exposure: Option<Exposure>,
        components: Vec<String>,
    ) -> BoxFuture<'_, Result<Endpoints>>;
    /// Run `script` in the stackbox's all-in-one or control VM and return its stdout. A
    /// non-zero exit status is an error carrying the script's stderr. Unlike `endpoints`,
    /// whose script is fixed, this lets the coordinator read and write instance metadata
    /// through the OpenStack CLI the VM already has.
    fn control_script(&self, _id: String, _script: String) -> BoxFuture<'_, Result<String>> {
        Box::pin(async { Err(anyhow!("this backend cannot run commands in a stackbox VM")) })
    }
    /// Stop the stackbox's VM, capture its disks under `labels`, and start it again. With
    /// `node`, only that VM is captured, as a control or compute template.
    fn freeze(
        &self,
        _id: String,
        _snapshot_id: String,
        _node: Option<String>,
        _labels: BTreeMap<String, String>,
        _sink: EventSink,
    ) -> BoxFuture<'_, Result<FrozenSnapshot>> {
        unsupported()
    }
    /// Boot the stackbox from a snapshot: `reference` for an all-in-one VM or a control
    /// template's control VM, `nodes` for a multi-node stackbox's VMs.
    fn restore(
        &self,
        _id: String,
        _reference: String,
        _nodes: Vec<stackbox::NodeSnapshot>,
        _plan: Plan,
        _exposure: Exposure,
    ) -> BoxFuture<'_, Result<Vec<StackboxNode>>> {
        unsupported()
    }
    /// Give a deployed stackbox's compute-1 the host's `vfio-pci` devices after it booted from a
    /// snapshot or joined with them (see `kolla::attach_pci`).
    fn attach_pci(
        &self,
        _id: String,
        _release: String,
        _pci_devices: Vec<String>,
        _mtty: bool,
        _sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported()
    }
    /// Wait for a deployed stackbox's services after a cold start, optionally moving its
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
    /// Drop the registrations of the source stackbox's computes from a stackbox restored from a
    /// control template, and list no compute in its Kolla inventory.
    fn forget_computes(
        &self,
        _id: String,
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
    /// The labels of the snapshot at `reference`, or `None` when it is gone.
    fn snapshot_labels(
        &self,
        _reference: String,
    ) -> BoxFuture<'_, Result<Option<BTreeMap<String, String>>>> {
        unsupported()
    }
    /// Add faults to the links of a multi-node stackbox's VM (`control`, `compute-1`, ...);
    /// `Impair::default()` heals them.
    fn impair(&self, _id: String, _node: String, _impair: Impair) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Err(anyhow!("this backend has no links")) })
    }
    /// Run an interactive login shell in the stackbox's VM until it exits or `io.input` closes.
    /// `node` picks a multi-node VM by suffix; `None` is the all-in-one or control VM.
    fn terminal(
        &self,
        _id: String,
        _node: Option<String>,
        _size: TerminalSize,
        _io: TerminalIo,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Err(anyhow!("this backend has no terminal")) })
    }
}
/// The stackboxes of the host its agent directory belongs to, whose link switch it runs.
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
    fn scan(&self) -> BoxFuture<'_, Result<BTreeMap<String, Vec<StackboxNode>>>> {
        Box::pin(stackbox::scan_stackboxes())
    }
    fn create(
        &self,
        id: String,
        plan: Plan,
        exposure: Exposure,
    ) -> BoxFuture<'_, Result<Vec<StackboxNode>>> {
        Box::pin(async move {
            stackbox::create_stackbox_with(
                &id,
                &plan.nodes,
                Some(&exposure),
                &plan.components,
                Some(&self.switch),
                &[],
                None,
                &plan.pci_devices,
            )
            .await
        })
    }
    fn create_share(
        &self,
        id: String,
        plan: Plan,
        exposure: Exposure,
        share: crate::remote::protocol::Share,
    ) -> BoxFuture<'_, Result<Vec<StackboxNode>>> {
        Box::pin(async move {
            stackbox::create_stackbox_with(
                &id,
                &plan.nodes,
                Some(&exposure),
                &plan.components,
                Some(&self.switch),
                &[],
                Some(&share),
                &plan.pci_devices,
            )
            .await
        })
    }
    fn create_share_from(
        &self,
        id: String,
        plan: Plan,
        exposure: Exposure,
        share: crate::remote::protocol::Share,
        source: String,
    ) -> BoxFuture<'_, Result<Vec<StackboxNode>>> {
        Box::pin(async move {
            anyhow::ensure!(
                share.nodes.len() == 1 && share.nodes[0].starts_with("compute-"),
                "a compute template boots a share of one compute"
            );
            // Only the share's VM is created here, so every entry may name the template.
            let sources = vec![source; plan.nodes.len()];
            stackbox::create_stackbox_with(
                &id,
                &plan.nodes,
                Some(&exposure),
                &plan.components,
                Some(&self.switch),
                &sources,
                Some(&share),
                &plan.pci_devices,
            )
            .await
        })
    }
    fn expand(
        &self,
        id: String,
        plan: Plan,
        _nodes: Vec<String>,
        add: Vec<String>,
        uplink: Option<crate::link::UplinkConfig>,
        source: Option<String>,
    ) -> BoxFuture<'_, Result<Vec<StackboxNode>>> {
        Box::pin(async move {
            if let Some(uplink) = uplink {
                self.switch.set_uplink(&id, uplink)?;
            }
            stackbox::add_nodes(
                &id,
                &plan.nodes,
                &add,
                &self.switch,
                source.as_deref(),
                &plan.pci_devices,
            )
            .await
        })
    }
    fn ssh_keys(&self, id: String) -> BoxFuture<'_, Result<(String, String)>> {
        Box::pin(async move {
            let node = stackbox::get_stackbox(&id)
                .await?
                .into_iter()
                .find(|node| node.role == Some(stackbox::Role::Control))
                .context("the stackbox has no control VM")?;
            let sandbox = stackbox::connect(&node.name).await?;
            let result = async {
                let private = crate::shell::run(
                    &sandbox,
                    "test -f /root/.ssh/id_ed25519 && cat /root/.ssh/id_ed25519",
                )
                .await
                .context("the control VM has no SSH key; deploy the stackbox first")?;
                let public = crate::shell::run(
                    &sandbox,
                    "test -f /root/.ssh/id_ed25519.pub && cat /root/.ssh/id_ed25519.pub",
                )
                .await
                .context("the control VM has no SSH public key")?;
                let private = private.trim().to_owned();
                let public = public.trim().to_owned();
                anyhow::ensure!(
                    private.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"),
                    "the control VM's SSH key is not OpenSSH private key material"
                );
                anyhow::ensure!(
                    public.starts_with("ssh-ed25519 "),
                    "the control VM's SSH public key is invalid"
                );
                Ok((public, private))
            }
            .await;
            stackbox::detach(&sandbox).await;
            result
        })
    }
    fn deploy_compute(
        &self,
        id: String,
        plan: Plan,
        index: u8,
        nodes: Vec<String>,
        components: Vec<String>,
        from_template: bool,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            kolla::deploy_compute(&id, &plan, index, &nodes, &components, from_template, sink).await
        })
    }
    fn relink(&self, id: String, url: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { self.switch.set_uplink_url(&id, &url) })
    }
    fn prepare(
        &self,
        id: String,
        components: Vec<String>,
        release: String,
        ssh_public_key: String,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(
            async move { kolla::prepare_share(&id, &components, &release, &ssh_public_key).await },
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn deploy_spanning(
        &self,
        id: String,
        from: Phase,
        exposure: Option<Exposure>,
        components: Vec<String>,
        release: String,
        reference: Option<String>,
        gpu: Option<crate::model::GpuConfig>,
        pci_devices: Vec<String>,
        remote: Vec<u8>,
        ssh_private_key: String,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            kolla::deploy_spanning(
                &id,
                from,
                exposure,
                &components,
                &release,
                reference.as_deref(),
                gpu,
                &pci_devices,
                &remote,
                &ssh_private_key,
                sink,
            )
            .await
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn deploy(
        &self,
        id: String,
        from: Phase,
        exposure: Option<Exposure>,
        components: Vec<String>,
        release: String,
        reference: Option<String>,
        gpu: Option<crate::model::GpuConfig>,
        pci_devices: Vec<String>,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            kolla::deploy_with(
                &id,
                from,
                exposure,
                &components,
                &release,
                reference.as_deref(),
                gpu,
                &pci_devices,
                sink,
            )
            .await
        })
    }
    fn remove_golden(&self) -> BoxFuture<'_, Result<usize>> {
        Box::pin(crate::stackbox::golden::remove_all())
    }
    fn delete(&self, id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            stackbox::delete_stackbox(&id).await?;
            self.switch.remove_ports(&id)?;
            Ok(())
        })
    }
    fn impair(&self, id: String, node: String, impair: Impair) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            for (_, network, _) in stackbox::NETWORKS {
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
            // New stackboxes keep Kolla's configuration on the all-in-one or control VM. Legacy
            // stackboxes remain readable/deletable; their credentials live on the old deployer.
            let name = match stackbox::all_in_one_name(&id).await {
                Ok(name) => name,
                Err(error) => stackbox::get_stackbox(&id)
                    .await?
                    .into_iter()
                    .find(|node| node.role == Some(stackbox::Role::Deployer))
                    .map(|node| node.name)
                    .ok_or(error)?,
            };
            let sandbox = stackbox::connect(&name).await?;
            let result = crate::shell::run(&sandbox, r#"set -euo pipefail
. /etc/kolla/admin-openrc.sh
python3 - <<'PY'
import os, json
keys = ['OS_AUTH_URL', 'OS_USERNAME', 'OS_PASSWORD', 'OS_PROJECT_NAME', 'OS_USER_DOMAIN_NAME', 'OS_PROJECT_DOMAIN_NAME', 'OS_REGION_NAME', 'OS_IDENTITY_API_VERSION', 'OS_INTERFACE']
print(json.dumps({k: os.environ[k] for k in keys if k in os.environ}))
PY"#).await;
            // The data stores report their own credentials and live schemas.
            let stores = crate::shell::run(&sandbox, DATASTORES_SCRIPT).await;
            stackbox::detach(&sandbox).await;
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
                relay_exposure: None,
            })
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
            match node.as_deref() {
                Some("control") => stackbox::freeze_control(&id, &snapshot_id, &labels, sink).await,
                Some(node) => {
                    stackbox::freeze_compute(&id, &snapshot_id, node, &labels, sink).await
                }
                None => stackbox::freeze_stackbox(&id, &snapshot_id, &labels, sink).await,
            }
        })
    }
    fn restore(
        &self,
        id: String,
        reference: String,
        nodes: Vec<stackbox::NodeSnapshot>,
        plan: Plan,
        exposure: Exposure,
    ) -> BoxFuture<'_, Result<Vec<StackboxNode>>> {
        Box::pin(async move {
            if plan.multinode() && nodes.is_empty() {
                // A control template holds the control VM alone.
                anyhow::ensure!(
                    plan.nodes.len() == 1,
                    "a control template restores the control VM alone"
                );
                return stackbox::create_stackbox_with(
                    &id,
                    &plan.nodes,
                    Some(&exposure),
                    &plan.components,
                    Some(&self.switch),
                    &[reference],
                    None,
                    &plan.pci_devices,
                )
                .await;
            }
            if plan.multinode() {
                // Each VM boots from the disks taken from the VM in its place.
                let sources = plan
                    .nodes
                    .iter()
                    .map(|spec| {
                        let suffix = stackbox::suffix(spec.role, spec.index);
                        nodes
                            .iter()
                            .find(|n| Some(&n.node) == suffix.as_ref())
                            .map(|n| n.reference.clone())
                            .with_context(|| format!("snapshot has no disks for {suffix:?}"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                return stackbox::create_stackbox_with(
                    &id,
                    &plan.nodes,
                    Some(&exposure),
                    &plan.components,
                    Some(&self.switch),
                    &sources,
                    None,
                    &plan.pci_devices,
                )
                .await;
            }
            let [spec] = plan.nodes[..] else {
                anyhow::bail!("snapshots hold exactly one all-in-one VM");
            };
            stackbox::restore_stackbox(&id, &reference, spec, &exposure, &plan.components).await
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
    fn attach_pci(
        &self,
        id: String,
        release: String,
        pci_devices: Vec<String>,
        mtty: bool,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { kolla::attach_pci(&id, &release, &pci_devices, mtty, sink).await })
    }
    fn forget_computes(
        &self,
        id: String,
        components: Vec<String>,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { kolla::forget_computes(&id, &components, sink).await })
    }
    fn export_snapshot(&self, reference: String, output: PathBuf) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { stackbox::export_snapshot(&reference, &output).await })
    }
    fn import_snapshot(
        &self,
        archive: PathBuf,
        snapshot_id: String,
    ) -> BoxFuture<'_, Result<ImportedSnapshot>> {
        Box::pin(async move { stackbox::import_snapshot(&archive, &snapshot_id).await })
    }
    fn remove_snapshot(&self, reference: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { stackbox::remove_snapshot(&reference).await })
    }
    fn snapshot_labels(
        &self,
        reference: String,
    ) -> BoxFuture<'_, Result<Option<BTreeMap<String, String>>>> {
        Box::pin(async move { stackbox::snapshot_labels(&reference).await })
    }
    fn control_script(&self, id: String, script: String) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move { stackbox::run_script(&id, None, &script).await })
    }
    fn terminal(
        &self,
        id: String,
        node: Option<String>,
        size: TerminalSize,
        io: TerminalIo,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            stackbox::attach_terminal(&id, node.as_deref(), size, io)
                .await
                .map(|_| ())
        })
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
