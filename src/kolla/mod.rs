mod config;
pub mod releases;

use std::fmt;

use anyhow::{Context, Result, bail};
use microsandbox::Sandbox;

use crate::model::Exposure;
use crate::shell::{run, stream};
use crate::stackbox;
pub use config::OPENSTACK_RELEASE;
/// Python the golden image ships, which the releases that still have a branch run on.
pub const GUEST_PYTHON: &str = "3.12";
use config::{Compute, GuestPciDevice, Topology};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug)]
pub enum DeployEvent {
    Phase(Phase),
    /// A finer step inside the phase: our own markers, or an Ansible task name.
    Step(String),
    /// A numbered step of a job whose steps are known up front, such as a freeze.
    Progress(StepProgress),
    Output(String),
}
/// Step `index` (from 1) of `total`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StepProgress {
    pub step: String,
    pub index: u32,
    pub total: u32,
}
pub type EventSink = Arc<dyn Fn(DeployEvent) -> Result<()> + Send + Sync>;

const VENV: &str = "/opt/kolla";
const OSC_VENV: &str = "/opt/osc";
const CONFIG_DIR: &str = "/etc/kolla";
const INVENTORY: &str = "/etc/kolla/inventory";
const LOG_DIR: &str = "/var/log/stackbox";
/// Neutron's external network lives behind the all-in-one host's `stackbox-extgw` veth.
const EXTERNAL_NETWORK: &str = "172.24.4.0/24";
const EXTERNAL_GATEWAY: &str = "172.24.4.1";
/// Replaces the vswitchd image's start hook, which loads the openvswitch kernel module
/// under `set -e`. The userspace datapath needs no module, and the guest kernel has none.
const OVS_EXTEND_START: &str = "/etc/stackbox/ovs-extend-start";
const OVS_EXTEND_START_SCRIPT: &str = r#"#!/bin/bash
# Written by stackbox: kolla's version also runs `modprobe openvswitch`.
if [[ ! -r "/var/log/kolla/openvswitch/ovs-vswitchd.log" ]]; then
    touch /var/log/kolla/openvswitch/ovs-vswitchd.log
    chown openvswitch:openvswitch /var/log/kolla/openvswitch/ovs-vswitchd.log
fi
"#;

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    clap::ValueEnum,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// Wait for systemd and prepare the all-in-one host
    #[default]
    Prepare,
    /// Install kolla-ansible and the OpenStack client on the all-in-one host
    Install,
    /// Write passwords, globals.yml and the inventory
    Configure,
    Bootstrap,
    Prechecks,
    Deploy,
    PostDeploy,
    /// Issue a token and round-trip an image through Glance
    Verify,
}

impl Phase {
    const ALL: [Phase; 8] = [
        Phase::Prepare,
        Phase::Install,
        Phase::Configure,
        Phase::Bootstrap,
        Phase::Prechecks,
        Phase::Deploy,
        Phase::PostDeploy,
        Phase::Verify,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Phase::Prepare => "prepare",
            Phase::Install => "install",
            Phase::Configure => "configure",
            Phase::Bootstrap => "bootstrap",
            Phase::Prechecks => "prechecks",
            Phase::Deploy => "deploy",
            Phase::PostDeploy => "post-deploy",
            Phase::Verify => "verify",
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub async fn deploy(stackbox_id: &str, from: Phase, components: &[String]) -> Result<()> {
    deploy_with(
        stackbox_id,
        from,
        None,
        components,
        OPENSTACK_RELEASE,
        None,
        None,
        &[],
        Arc::new(|_| Ok(())),
    )
    .await
}

/// `components` is the stackbox plan's resolved service list, and `reference` the kolla-ansible
/// ref the coordinator resolved for `release` (see [`releases::info_with`]).
#[allow(clippy::too_many_arguments)]
pub async fn deploy_with(
    stackbox_id: &str,
    from: Phase,
    exposure: Option<Exposure>,
    components: &[String],
    release: &str,
    reference: Option<&str>,
    gpu: Option<crate::model::GpuConfig>,
    pci_devices: &[String],
    sink: EventSink,
) -> Result<()> {
    deploy_on(
        stackbox_id,
        from,
        exposure,
        components,
        release,
        reference,
        gpu,
        pci_devices,
        &[],
        None,
        sink,
    )
    .await
}

/// Deploy a stackbox that spans hosts from its control VM's host. The compute VMs numbered in
/// `remote` run on other hosts, which [`prepare_share`] readied; Kolla reaches every compute
/// with `ssh_private_key`.
#[allow(clippy::too_many_arguments)]
pub async fn deploy_spanning(
    stackbox_id: &str,
    from: Phase,
    exposure: Option<Exposure>,
    components: &[String],
    release: &str,
    reference: Option<&str>,
    gpu: Option<crate::model::GpuConfig>,
    pci_devices: &[String],
    remote: &[u8],
    ssh_private_key: &str,
    sink: EventSink,
) -> Result<()> {
    deploy_on(
        stackbox_id,
        from,
        exposure,
        components,
        release,
        reference,
        gpu,
        pci_devices,
        remote,
        Some(ssh_private_key),
        sink,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn deploy_on(
    stackbox_id: &str,
    from: Phase,
    exposure: Option<Exposure>,
    components: &[String],
    release: &str,
    reference: Option<&str>,
    gpu: Option<crate::model::GpuConfig>,
    pci_devices: &[String],
    remote: &[u8],
    ssh_private_key: Option<&str>,
    sink: EventSink,
) -> Result<()> {
    let mut nodes = stackbox::get_stackbox(stackbox_id).await?;
    // Computes on other hosts take their place in the layout; they are reached over SSH only.
    nodes.extend(remote.iter().map(|index| stackbox::StackboxNode {
        name: String::new(),
        role: Some(stackbox::Role::Compute),
        index: Some(*index),
        status: "running".into(),
        memory_mib: 0,
        ip: None,
    }));
    let mut topology = Topology::from_nodes(&nodes)?;
    let deployer = stackbox::connect(&topology.name).await?;
    let mut computes = Vec::new();
    let result = async {
        for compute in &topology.computes {
            let sandbox = if compute.name.is_empty() {
                None
            } else {
                Some(stackbox::connect(&compute.name).await?)
            };
            computes.push((compute.clone(), sandbox));
        }
        if let Some(key) = ssh_private_key {
            install_ssh_key(&deployer, key).await?;
        }
        stackbox::wait_until_booted(&deployer).await?;
        // Stackboxes created with a fixed guest identity bind every service to it; older stackboxes use eth0.
        let identity = run(&deployer, &identity_command()).await?;
        let (interface, address) = identity
            .trim()
            .split_once(' ')
            .context("cannot read the guest service address")?;
        topology.set_address(address)?;
        topology.interface = interface.to_owned();
        deploy_phases(
            stackbox_id,
            from,
            &deployer,
            &computes,
            &topology,
            exposure.as_ref(),
            components,
            release,
            reference,
            gpu.as_ref(),
            pci_devices,
            &sink,
        )
        .await
    }
    .await;
    stackbox::detach(&deployer).await;
    for (_, sandbox) in computes.into_iter().filter_map(|(c, s)| Some((c, s?))) {
        stackbox::detach(&sandbox).await;
    }
    result
}

/// Reads the guest's service interface and address on one line: the fixed identity a stackbox
/// created with one binds to, or `eth0` on stackboxes made before it.
fn identity_command() -> String {
    format!(
        "if [ -f {conf} ]; then . {conf}; echo \"{dummy} $STACKBOX_ADDRESS\"; else echo \"eth0 $(ip -4 -o addr show dev eth0 | awk '!seen++ {{split($4,a,\"/\"); print a[1]}}')\"; fi",
        conf = stackbox::IDENTITY_CONF,
        dummy = stackbox::GUEST_INTERFACE,
    )
}

/// Give the control VM the key Kolla reaches the computes with.
/// Add one compute VM to a deployed stackbox with Kolla. `nodes` are the stackbox's VMs on this
/// host; computes on other hosts were prepared there. Everything is limited to the new
/// compute, so the control plane keeps running. A compute booted from a compute template
/// (`from_template`) already has Kolla's containers: its identity is wiped and Kolla only
/// reconfigures it, skipping bootstrap-servers and the image pull.
pub async fn deploy_compute(
    stackbox_id: &str,
    plan: &crate::model::Plan,
    index: u8,
    nodes: &[String],
    components: &[String],
    from_template: bool,
    sink: EventSink,
) -> Result<()> {
    let control_spec = plan
        .nodes
        .iter()
        .find(|spec| spec.role == stackbox::Role::Control)
        .context("a compute can only join a stackbox with a control VM")?;
    let new_suffix = format!("compute-{index}");
    let new_spec = plan
        .nodes
        .iter()
        .find(|spec| spec.role == stackbox::Role::Compute && spec.index == Some(index))
        .with_context(|| format!("stackbox plan has no {new_suffix}"))?;
    let release = releases::info(plan.release.as_deref().unwrap_or(OPENSTACK_RELEASE));
    let control_name = stackbox::node_name(stackbox_id, stackbox::Role::Control, None);
    let control = stackbox::connect(&control_name).await?;
    let mut sandboxes: Vec<Sandbox> = Vec::new();
    let result = async {
        stackbox::wait_until_booted(&control).await?;
        let identity = run(&control, &identity_command()).await?;
        let (interface, address) = identity
            .trim()
            .split_once(' ')
            .context("cannot read the guest service address")?;
        let mut topology = Topology {
            name: control_name.clone(),
            ip: Some(address.to_owned()),
            interface: interface.to_owned(),
            computes: plan
                .nodes
                .iter()
                .filter(|spec| spec.role == stackbox::Role::Compute)
                .map(|spec| Compute {
                    name: stackbox::node_name(stackbox_id, spec.role, spec.index),
                    hostname: spec.hostname(),
                    address: spec.address(),
                })
                .collect(),
            multinode: true,
        };
        topology
            .computes
            .sort_by(|a, b| a.hostname.cmp(&b.hostname));
        step(
            &sink,
            format!("control VM에 {new_suffix}의 hostname·주소 반영"),
        )?;
        stackbox::update_identity(&control, control_spec, &plan.nodes).await?;
        let neutron = components.iter().any(|c| c == "neutron");
        let mut computes: Vec<(Compute, Option<Sandbox>)> = Vec::new();
        for compute in &topology.computes {
            let suffix = compute
                .hostname
                .strip_prefix("stackbox-")
                .context("compute hostname has no role prefix")?;
            if !nodes.iter().any(|node| node == suffix) {
                computes.push((compute.clone(), None));
                continue;
            }
            let sandbox = stackbox::connect(&compute.name).await?;
            if suffix == new_suffix && from_template {
                step(&sink, format!("{new_suffix}의 템플릿 identity 초기화"))?;
                stackbox::wait_until_booted(&sandbox).await?;
                stackbox::update_identity(&sandbox, new_spec, &plan.nodes).await?;
                run(&sandbox, &wipe_template_script(&new_spec.hostname())).await?;
            } else if suffix == new_suffix {
                step(&sink, format!("{new_suffix} prepare"))?;
                prepare_host(&sandbox, neutron, &release, &sink).await?;
            }
            computes.push((compute.clone(), Some(sandbox)));
        }
        for (_, sandbox) in computes.iter() {
            if let Some(sandbox) = sandbox {
                sandboxes.push(sandbox.clone());
            }
        }
        // A GPU stackbox's new compute must boot the same custom kernel as the others.
        if plan
            .gpu
            .as_ref()
            .is_some_and(|gpu| gpu.mode == crate::model::GpuMode::Mtty)
        {
            let gpu = plan.gpu.as_ref().expect("checked above");
            step(&sink, "compute VM의 mtty mdev 장치 확인")?;
            check_mdev(&control, &computes, gpu.count).await?;
        }
        step(&sink, "control VM에서 새 compute로 SSH 키 배포")?;
        distribute_ssh_key(&control, &computes).await?;
        step(&sink, "Kolla multinode inventory 갱신")?;
        let sources = compute_image_sources(&control, &computes, &release).await?;
        write_inventory(&control, &topology, &sources).await?;
        let host = new_spec.hostname();
        if from_template {
            sink(DeployEvent::Phase(Phase::Deploy))?;
            if nova_mtty_backport(plan.gpu.as_ref(), &release.release) {
                step(
                    &sink,
                    format!("{host}의 nova-compute mtty 백포트 파일 준비"),
                )?;
                let new = computes
                    .iter()
                    .filter(|(compute, _)| compute.hostname == host)
                    .cloned()
                    .collect::<Vec<_>>();
                install_nova_mtty_files(&control, true, &new, &release.release).await?;
            }
            step(&sink, format!("kolla-ansible reconfigure --limit {host}"))?;
            stream(
                &control,
                &kolla_ansible_limited("reconfigure", &host, &release),
                &format!("{LOG_DIR}/add-compute-{index}-reconfigure.log"),
                &sink,
            )
            .await?;
        } else {
            sink(DeployEvent::Phase(Phase::Bootstrap))?;
            step(
                &sink,
                format!("kolla-ansible bootstrap-servers --limit {host}"),
            )?;
            stream(
                &control,
                &kolla_ansible_limited("bootstrap-servers", &host, &release),
                &format!("{LOG_DIR}/add-compute-{index}-bootstrap.log"),
                &sink,
            )
            .await?;
            sink(DeployEvent::Phase(Phase::Deploy))?;
            if nova_mtty_backport(plan.gpu.as_ref(), &release.release) {
                step(
                    &sink,
                    format!("{host}의 nova-compute mtty 백포트 파일 준비"),
                )?;
                let new = computes
                    .iter()
                    .filter(|(compute, _)| compute.hostname == host)
                    .cloned()
                    .collect::<Vec<_>>();
                install_nova_mtty_files(&control, true, &new, &release.release).await?;
            }
            step(&sink, format!("kolla-ansible deploy --limit {host}"))?;
            stream(
                &control,
                &kolla_ansible_limited("deploy", &host, &release),
                &format!("{LOG_DIR}/add-compute-{index}-deploy.log"),
                &sink,
            )
            .await?;
        }
        // Kolla restarts the control VM's OVS containers when the stackbox gains a compute, and the
        // agent container that starts with them can find the database not yet listening and die.
        // A compute would then join a stackbox whose network node has no agent, so give it one.
        step(&sink, "control VM의 OVS agent 확인")?;
        run(&control, RESTART_OVS_AGENT).await?;
        sink(DeployEvent::Phase(Phase::Verify))?;
        step(&sink, format!("{host}의 nova-compute·network agent 확인"))?;
        stream(
            &control,
            &verify_compute_script(&host, neutron),
            &format!("{LOG_DIR}/add-compute-{index}-verify.log"),
            &sink,
        )
        .await?;
        if from_template {
            // deploy maps a new compute to its cell; reconfigure does not.
            step(&sink, "nova-manage cell_v2 discover_hosts")?;
            stream(
                &control,
                "docker exec nova_api nova-manage cell_v2 discover_hosts --verbose",
                &format!("{LOG_DIR}/add-compute-{index}-discover.log"),
                &sink,
            )
            .await?;
        }
        // A GPU stackbox's providers are stackbox's own, so a compute that joins later gets them
        // here; the stackbox's deploy wrote the earlier computes'.
        let advertise = gpu_advertise_script(plan.gpu.as_ref(), std::slice::from_ref(&host));
        if !advertise.is_empty() {
            step(&sink, format!("{host}의 GPU resource provider 광고"))?;
            stream(
                &control,
                &advertise,
                &format!("{LOG_DIR}/add-compute-{index}-gpu.log"),
                &sink,
            )
            .await?;
        }
        // A compute-1 that joins with passed-through devices gets them once its nova roles are up.
        if index == 1 && !plan.pci_devices.is_empty() {
            let mtty = plan
                .gpu
                .as_ref()
                .is_some_and(|gpu| gpu.mode == crate::model::GpuMode::Mtty);
            attach_pci_on(
                &control,
                &computes,
                &plan.pci_devices,
                mtty,
                &release,
                &sink,
            )
            .await?;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    stackbox::detach(&control).await;
    for sandbox in sandboxes {
        stackbox::detach(&sandbox).await;
    }
    result
}

/// Make a stackbox restored from a control template forget the computes of the stackbox it was
/// frozen from, so its first expansion is compute-1 again. Only this copy changes; the
/// source stackbox keeps its computes. Kolla's inventory lists no compute, the control VM forgets
/// their SSH host keys, and OpenStack drops their registrations: each nova-compute service
/// (Nova removes its compute node, placement resource provider and cell host mapping with
/// it), any of those left behind, and their Neutron agents. Waits for the APIs first, and
/// is safe to run again.
pub async fn forget_computes(
    stackbox_id: &str,
    components: &[String],
    sink: EventSink,
) -> Result<()> {
    let name = stackbox::node_name(stackbox_id, stackbox::Role::Control, None);
    let control = stackbox::connect(&name).await?;
    let result = async {
        stackbox::wait_until_booted(&control).await?;
        step(&sink, "Kolla multinode inventory에서 원본 compute 제거")?;
        let topology = Topology {
            name: name.clone(),
            ip: None,
            interface: stackbox::GUEST_INTERFACE.into(),
            computes: Vec::new(),
            multinode: true,
        };
        write_inventory(&control, &topology, &BTreeMap::new()).await?;
        step(&sink, "원본 stackbox의 compute 등록 정리")?;
        stream(
            &control,
            &forget_computes_script(components),
            &format!("{LOG_DIR}/forget-computes.log"),
            &sink,
        )
        .await
    }
    .await;
    stackbox::detach(&control).await;
    result
}

/// Compute VMs are `stackbox-compute-<n>` to OpenStack; the control VM is never matched.
fn forget_computes_script(components: &[String]) -> String {
    let has = |id: &str| components.iter().any(|c| c == id);
    let known_hosts: String = (1..=stackbox::MAX_COMPUTES)
        .map(|index| {
            format!(
                "ssh-keygen -R {} >/dev/null 2>&1 || true
ssh-keygen -R {} >/dev/null 2>&1 || true
",
                stackbox::hostname(stackbox::Role::Compute, Some(index)),
                stackbox::address(stackbox::Role::Compute, Some(index)),
            )
        })
        .collect();
    let mut script = format!(
        r#"set -euo pipefail
{known_hosts}. {CONFIG_DIR}/admin-openrc.sh
os={OSC_VENV}/bin/openstack
ok=''
for _ in $(seq 120); do
  if $os token issue -f value -c expires >/dev/null 2>&1; then ok=1; break; fi
  sleep 5
done
[ -n "$ok" ] || {{ echo "keystone did not answer within 10 minutes" >&2; exit 1; }}
"#
    );
    if has("nova") {
        script.push_str(&format!(
            r#"for _ in $(seq 60); do $os compute service list -f value -c ID >/dev/null 2>&1 && break; sleep 5; done
# Nova deletes a service's compute nodes, their resource providers and its host mapping with it.
$os compute service list --service nova-compute -f value -c ID -c Host | while read -r id host; do
  case "$host" in stackbox-compute-*) ;; *) continue ;; esac
  echo "nova-compute $host: deleting service $id"
  $os compute service delete "$id"
done
# The OpenStack CLI has no placement commands without osc-placement; its SDK has them.
{OSC_VENV}/bin/python - <<'PY'
import openstack
conn = openstack.connect()
# A GPU stackbox's child providers are named after their compute's provider, so deleting in
# reverse name order takes every child before the provider it hangs from: Placement refuses
# to delete a provider that still has children, and refuses one that still has allocations.
for rp in sorted(
    (
        provider
        for provider in conn.placement.resource_providers()
        if provider.name.startswith("stackbox-compute-")
    ),
    key=lambda provider: provider.name,
    reverse=True,
):
    print(f"placement: deleting resource provider {{rp.name}} {{rp.id}}", flush=True)
    conn.placement.delete_resource_provider(rp)
PY
docker exec nova_api nova-manage cell_v2 list_hosts | awk -F'|' 'NF >= 4 {{ gsub(/ /, "", $3); gsub(/ /, "", $4); if ($4 ~ /^stackbox-compute-/) print $3, $4 }}' | while read -r cell host; do
  echo "cell host mapping $host: deleting"
  docker exec nova_api nova-manage cell_v2 delete_host --cell_uuid "$cell" --host "$host"
done
"#
        ));
    }
    if has("neutron") {
        script.push_str(
            r#"$os network agent list -f value -c ID -c Host | while read -r id host; do
  case "$host" in stackbox-compute-*) ;; *) continue ;; esac
  echo "neutron agent on $host: deleting $id"
  $os network agent delete "$id"
done
"#,
        );
    }
    script.push_str("echo \"the source stackbox's computes are forgotten\"\n");
    script
}

/// Rewrite only Kolla's multinode inventory and Ansible concurrency, leaving passwords and
/// globals alone so an expansion cannot disturb the running control plane. The one globals
/// change is a stackbox deployed before computes could differ in architecture: its pinned
/// image tag suffix and registry become the per-host ones the inventory's `sources` feed.
async fn write_inventory(
    deployer: &Sandbox,
    topology: &Topology,
    sources: &BTreeMap<String, config::ImageSource>,
) -> Result<()> {
    let template = deployer
        .fs()
        .read_to_string(&format!(
            "{VENV}/share/kolla-ansible/ansible/inventory/multinode"
        ))
        .await?;
    let inventory = config::multinode_inventory(
        &template,
        &stackbox::hostname(stackbox::Role::Control, None),
        &topology.computes,
        sources,
    )?;
    let fs = deployer.fs();
    let globals_path = format!("{CONFIG_DIR}/globals.yml");
    if let Ok(globals) = fs.read_to_string(&globals_path).await
        && let Some(rewritten) = config::per_host_images(&globals)
    {
        fs.write(&globals_path, rewritten).await?;
    }
    fs.write(INVENTORY, inventory).await?;
    run(deployer, "mkdir -p /etc/kolla/callback_plugins").await?;
    fs.write(
        "/etc/kolla/callback_plugins/stackbox.py",
        include_str!("stackbox_callback.py"),
    )
    .await?;
    let forks = 1 + topology.computes.len();
    fs.write(
        "/root/.ansible.cfg",
        &format!(
            "[defaults]\nhost_key_checking = False\nforks = {forks}\nstdout_callback = stackbox\ncallback_plugins = /etc/kolla/callback_plugins\n\n[ssh_connection]\npipelining = True\n"
        ),
    )
    .await?;
    Ok(())
}

/// Make a compute booted from a compute template its own before its Docker starts: drop the
/// source's Nova `compute_id`, instances, libvirt domains and SSH keys, and give it a new
/// machine ID, which Kolla's libvirt host UUID follows on reconfigure. This runs once per
/// VM, so a retried expansion keeps the identity Nova registered. Docker then starts the
/// template's containers, and OVS takes the new hostname as its system-id.
fn wipe_template_script(hostname: &str) -> String {
    format!(
        r#"set -euo pipefail
marker=/etc/stackbox/template-wiped
if [ ! -e $marker ]; then
  systemctl stop docker.socket docker.service 2>/dev/null || true
  volumes=/var/lib/docker/volumes
  rm -f $volumes/nova_compute/_data/compute_id
  rm -rf $volumes/nova_compute/_data/instances
  rm -f $volumes/nova_libvirt_qemu/_data/*.xml $volumes/nova_libvirt_qemu/_data/autostart/*.xml
  rm -f /etc/machine-id /var/lib/dbus/machine-id
  systemd-machine-id-setup
  : > /root/.ssh/authorized_keys 2>/dev/null || true
  touch $marker
fi
systemctl enable docker.service docker.socket
systemctl start docker.service
# A container keeps the hostname it was created with, and the template made them all with
# the source VM's. Kolla's OVS agent takes its host from inside its container, so a copy that
# kept it would register under the source compute. Dropping them lets reconfigure make them
# again with this VM's hostname; their data is in volumes, which stay.
me=$(hostname)
for c in $(docker ps -a --format '{{{{.Names}}}}'); do
  case "$c" in openvswitch_db | openvswitch_vswitchd) continue ;; esac
  h=$(docker inspect -f '{{{{.Config.Hostname}}}}' "$c" 2>/dev/null || true)
  if [ -n "$h" ] && [ "$h" != "$me" ]; then
    echo "recreate $c: the template made it as $h"
    docker rm -f "$c" >/dev/null
  fi
done
if docker ps -a --format '{{{{.Names}}}}' | grep -x openvswitch_db >/dev/null; then
  for _ in $(seq 60); do docker exec openvswitch_db ovs-vsctl show >/dev/null 2>&1 && break; sleep 2; done
  docker exec openvswitch_db ovs-vsctl set Open_vSwitch . external_ids:system-id={hostname}
fi"#
    )
}

/// Wait for OVS on the control VM to answer, then restart its OVS agent if it is not healthy.
/// Kolla's agent container exits when it starts while the database is still coming up, and
/// nothing restarts it afterwards; the stackbox would keep a network node without an agent.
const RESTART_OVS_AGENT: &str = r#"for _ in $(seq 40); do
  docker ps --format '{{.Names}} {{.Status}}' | grep '^openvswitch_db Up .* (healthy)' >/dev/null && break
  sleep 2
done
if ! docker ps --format '{{.Names}} {{.Status}}' | grep '^neutron_openvswitch_agent Up .* (healthy)' >/dev/null; then
  docker restart neutron_openvswitch_agent >/dev/null 2>&1 || true
fi"#;

/// The environment a bash `kolla-ansible` launcher needs. Up to 2024.1 it finds the playbooks
/// in `$VIRTUAL_ENV/share/kolla-ansible`, or, with no `VIRTUAL_ENV`, in what it takes for a
/// source checkout next to its own `bin/`; the stackbox's phase scripts never activate the venv,
/// so those releases only work when the variable names it. 2024.2 replaced the launcher with a
/// Python console script that finds its own package.
fn venv_env(release: &releases::ReleaseInfo) -> &'static str {
    if release.venv_launcher() {
        "VIRTUAL_ENV=/opt/kolla "
    } else {
        ""
    }
}

fn kolla_ansible_limited(command: &str, host: &str, release: &releases::ReleaseInfo) -> String {
    format!(
        "{env}PATH={VENV}/bin:$PATH kolla-ansible {command} -i {INVENTORY} --limit {host}",
        env = venv_env(release)
    )
}

fn verify_compute_script(host: &str, neutron: bool) -> String {
    let mut script = format!(
        r#"set -euo pipefail
. {CONFIG_DIR}/admin-openrc.sh
os={OSC_VENV}/bin/openstack
errf=$(mktemp)
trap 'rm -f "$errf"' EXIT
state=''
err=''
for _ in $(seq 60); do
  rc=0
  out=$($os compute service list --service nova-compute --host {host} -f value -c State 2>"$errf") || rc=$?
  if [ "$rc" -eq 0 ]; then
    state=$out
    err=''
  else
    state=''
    err=$(awk 'NF {{ last = $0 }} END {{ print last }}' "$errf")
  fi
  [ "$state" = up ] && break
  sleep 5
done
[ "$state" = up ] || {{ echo "nova-compute {host} is not up: ${{state:-missing}}${{err:+ (last error: $err)}}" >&2; exit 1; }}
echo "nova-compute {host} up"
"#
    );
    if neutron {
        // `--agent-type` takes a choice of short names that change between client versions
        // (`open-vswitch` now, the display name before); the host's agents are few, so their
        // own type column decides.
        script.push_str(&format!(
            r#"alive=''
err=''
for _ in $(seq 60); do
  rc=0
  out=$($os network agent list --host {host} -f value -c "Agent Type" -c Alive 2>"$errf") || rc=$?
  if [ "$rc" -eq 0 ]; then
    alive=$(printf '%s\n' "$out" | awk '$1 == "Open" && $2 == "vSwitch" && !seen++ {{ print $NF }}')
    err=''
  else
    alive=''
    err=$(awk 'NF {{ last = $0 }} END {{ print last }}' "$errf")
  fi
  [ "$alive" = True ] && break
  sleep 5
done
[ "$alive" = True ] || {{ echo "Open vSwitch agent on {host} is not alive: ${{alive:-missing}}${{err:+ (last error: $err)}}" >&2; exit 1; }}
echo "Open vSwitch agent on {host} alive"
"#
        ));
    }
    script
}

async fn install_ssh_key(deployer: &Sandbox, key: &str) -> Result<()> {
    anyhow::ensure!(
        key.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"),
        "invalid SSH private key"
    );
    run(deployer, "mkdir -p /root/.ssh && chmod 700 /root/.ssh").await?;
    deployer.fs().write("/root/.ssh/id_ed25519", key).await?;
    run(
        deployer,
        "chmod 600 /root/.ssh/id_ed25519 && ssh-keygen -y -f /root/.ssh/id_ed25519 > /root/.ssh/id_ed25519.pub",
    )
    .await?;
    Ok(())
}

/// Ready this host's compute VMs of a stackbox that spans hosts, as the prepare phase readies
/// computes, and let `ssh_public_key` in as root.
pub async fn prepare_share(
    stackbox_id: &str,
    components: &[String],
    release: &str,
    ssh_public_key: &str,
) -> Result<()> {
    let neutron = components.iter().any(|c| c == "neutron");
    let release = releases::info(release);
    let sink: EventSink = Arc::new(|_| Ok(()));
    for node in stackbox::get_stackbox(stackbox_id).await? {
        if node.role != Some(stackbox::Role::Compute) {
            continue;
        }
        let sandbox = stackbox::connect(&node.name).await?;
        let result = async {
            prepare_host(&sandbox, neutron, &release, &sink).await?;
            authorize_key(&sandbox, ssh_public_key).await
        }
        .await;
        stackbox::detach(&sandbox).await;
        result.with_context(|| format!("prepare {}", node.name))?;
    }
    Ok(())
}

/// Reports a step inside the running phase.
fn step(sink: &EventSink, text: impl Into<String>) -> Result<()> {
    sink(DeployEvent::Step(text.into()))
}

#[allow(clippy::too_many_arguments)]
async fn deploy_phases(
    stackbox_id: &str,
    from: Phase,
    deployer: &Sandbox,
    computes: &[(Compute, Option<Sandbox>)],
    topology: &Topology,
    exposure: Option<&Exposure>,
    components: &[String],
    release: &str,
    reference: Option<&str>,
    gpu: Option<&crate::model::GpuConfig>,
    pci_devices: &[String],
    sink: &EventSink,
) -> Result<()> {
    // The coordinator resolved the kolla-ansible ref against the catalog; this process has
    // none, so every step below installs and stamps the ref it sent.
    let info = releases::info_with(release, reference);
    for phase in Phase::ALL.into_iter().filter(|p| *p >= from) {
        sink(DeployEvent::Phase(phase))?;
        eprintln!("\n==> [{stackbox_id}] {phase}");
        let log = format!("{LOG_DIR}/{phase}.log");
        match phase {
            Phase::Prepare => prepare(deployer, computes, components, &info, sink).await,
            Phase::Install => stream(deployer, &install_script(&info), &log, sink).await,
            Phase::Configure => {
                configure(
                    deployer,
                    computes,
                    topology,
                    exposure,
                    components,
                    release,
                    gpu,
                    pci_devices,
                    sink,
                )
                .await
            }
            Phase::Bootstrap => {
                step(
                    sink,
                    "kolla-ansible bootstrap-servers (컨테이너 호스트 초기화)",
                )?;
                stream(
                    deployer,
                    &kolla_ansible("bootstrap-servers", &info),
                    &log,
                    sink,
                )
                .await
            }
            Phase::Prechecks => {
                step(sink, "kolla-ansible prechecks")?;
                stream(deployer, &kolla_ansible("prechecks", &info), &log, sink).await
            }
            Phase::Deploy => {
                if nova_mtty_backport(gpu, release) {
                    step(
                        sink,
                        format!("nova-compute의 mtty 백포트 파일 준비 ({release})"),
                    )?;
                    install_nova_mtty_files(deployer, topology.multinode(), computes, release)
                        .await?;
                }
                step(sink, "kolla-ansible deploy")?;
                stream(deployer, &deploy_script(&info), &log, sink).await
            }
            Phase::PostDeploy => {
                step(sink, "kolla-ansible post-deploy (admin-openrc.sh 생성)")?;
                let script = post_deploy_script(components, topology.multinode(), &info);
                stream(deployer, &script, &log, sink).await
            }
            Phase::Verify => {
                step(sink, "배포 검증 시작")?;
                // An all-in-one VM runs nova-compute itself.
                let hosts = if topology.multinode() {
                    topology.computes.len()
                } else {
                    1
                };
                let mut script = verify_script(components, hosts, release, gpu);
                // A compute on a host of another architecture boots the other Cirros.
                if let Some(arch) = instance_arch(deployer, computes).await? {
                    script = format!("instance_arch={arch}\n{script}");
                }
                if !pci_devices.is_empty() {
                    step(
                        sink,
                        "PCI passthrough 확인 (compute-1 장치와 stackbox.pci 인스턴스)",
                    )?;
                    let guest = probe_guest_pci(deployer, computes, pci_devices.len()).await?;
                    script.push_str(&pci_verify_script(PCI_COMPUTE, &guest));
                }
                stream(deployer, &script, &log, sink).await
            }
        }
        .with_context(|| resume_hint(stackbox_id, phase))?;
        sink(DeployEvent::Output(format!("phase {phase} completed\n")))?;
    }
    let url = if let Some(exposure) = exposure {
        exposure.urls(Some(components))["keystone"].clone()
    } else {
        format!("http://{}:5000/v3", topology.vip()?)
    };
    eprintln!(
        "\n==> [{stackbox_id}] ready: {url} (credentials: {CONFIG_DIR}/admin-openrc.sh on {})",
        topology.name
    );
    Ok(())
}

/// Ready every VM for Kolla. The deployer (all-in-one or control VM) also hosts Neutron's
/// external network; a multi-node stackbox's computes only run OVS, Nova and Docker.
async fn prepare(
    deployer: &Sandbox,
    computes: &[(Compute, Option<Sandbox>)],
    components: &[String],
    release: &releases::ReleaseInfo,
    sink: &EventSink,
) -> Result<()> {
    let neutron = components.iter().any(|c| c == "neutron");
    prepare_host(deployer, neutron, release, sink).await?;
    // Computes on other hosts were prepared there.
    for (compute, sandbox) in computes.iter().filter_map(|(c, s)| Some((c, s.as_ref()?))) {
        step(sink, format!("{} 준비", compute.hostname))?;
        prepare_host(sandbox, neutron, release, sink).await?;
    }
    if neutron {
        step(sink, "Neutron 외부 네트워크(veth·nftables) 준비")?;
        deployer
            .fs()
            .write(
                "/etc/stackbox/external.conf",
                format!(
                    "EXTERNAL_NETWORK={EXTERNAL_NETWORK}\nEXTERNAL_GATEWAY={EXTERNAL_GATEWAY}/24\n"
                ),
            )
            .await?;
        deployer
            .fs()
            .write(
                "/etc/stackbox/neutron-sitecustomize.py",
                include_str!("neutron_sitecustomize.py"),
            )
            .await?;
        // Golden images now install nftables; stackboxes made from the former Docker-built
        // image may predate it.
        run(deployer, "command -v nft >/dev/null || dnf install -y -q nftables; systemctl restart stackbox-external.service").await?;
    }
    if !computes.is_empty() {
        step(sink, "control VM의 시스템 Python에 docker 모듈 설치")?;
        // Tasks the computes delegate to the control VM (nova-cell's hypervisor and cell
        // lists) run under the computes' interpreter, /usr/bin/python3, while bootstrap puts
        // the docker SDK into the control VM's Ansible venv only. dbus comes with the image.
        run(
            deployer,
            "/usr/bin/python3 -c 'import docker' 2>/dev/null || /usr/bin/python3 -m pip install -q docker",
        )
        .await?;
    }
    Ok(())
}

async fn prepare_host(
    sandbox: &Sandbox,
    neutron: bool,
    release: &releases::ReleaseInfo,
    sink: &EventSink,
) -> Result<()> {
    step(sink, "게스트 부팅과 systemd 대기")?;
    stackbox::wait_until_booted(sandbox).await?;
    step(sink, "stackbox-identity·stackbox-hostname 유닛 적용")?;
    // Stackboxes made by an older build get the current units too. msb resets /etc/hosts on
    // reboot, so restore the hostname mapping before Docker creates host-network containers.
    stackbox::install_guest_files(sandbox).await?;
    run(
        sandbox,
        "systemctl restart stackbox-identity.service stackbox-hostname.service",
    )
    .await?;
    // Kolla's `rpm_key` task for the Docker repository runs `gpg2`, which only `gnupg2`
    // provides and this guest's base image leaves out, whatever the series.
    step(sink, "게스트 툴링 호환 패키지 준비 (gnupg2)")?;
    run(
        sandbox,
        "rpm -q gnupg2 >/dev/null 2>&1 || dnf install -y -q gnupg2",
    )
    .await?;
    if release.eol {
        // Ansible 2.9 loads its facts and modules through `distutils`, which Python 3.12
        // removed and setuptools provides again through a path hook.
        step(sink, "EOL 툴링 호환 패키지 준비 (python3-setuptools)")?;
        run(
            sandbox,
            "rpm -q python3-setuptools >/dev/null 2>&1 || dnf install -y -q python3-setuptools",
        )
        .await?;
        // Kolla's own modules run under the guest's interpreter and import the Docker SDK.
        // The tag's bootstrap installs it through `pip3`, which on the control VM resolves
        // into the kolla-ansible venv; these tags pin the SDK below 5.0.0, which needs
        // urllib3 1.x and requests before 2.32 to reach the daemon over `http+docker`.
        run(
            sandbox,
            "/usr/bin/python3 -m pip install -q 'docker>=3.4.1,<5.0.0' 'urllib3<2' 'requests<2.32'",
        )
        .await
        .context("docker SDK for the guest's system python")?;
    }
    step(sink, "Docker 마운트 전파와 /dev/kvm 권한 준비")?;
    // Kolla toolbox bind-mounts /run with shared propagation, and nova_libvirt runs QEMU
    // as uid/gid 42427 without systemd-udevd in the guest. Apply both now and on every Docker
    // start, which also waits for a multi-node stackbox's links.
    run(
        sandbox,
        r#"set -euo pipefail
mkdir -p /etc/udev/rules.d /etc/systemd/system/docker.service.d
cat > /etc/systemd/system/docker.service.d/stackbox-mounts.conf <<'EOF'
[Unit]
Requires=stackbox-hostname.service
After=stackbox-link@stackbox0.service stackbox-link@stackbox1.service stackbox-identity.service stackbox-hostname.service stackbox-external.service

[Service]
ExecStartPre=/usr/bin/mount --make-shared /run
ExecStartPre=/usr/bin/sh -c 'if [ -c /dev/kvm ]; then chown root:42427 /dev/kvm && chmod 0666 /dev/kvm; fi'
EOF
mount --make-shared /run
if [ -c /dev/kvm ]; then chown root:42427 /dev/kvm && chmod 0666 /dev/kvm; fi
# A compute with passed-through devices: nova_libvirt opens the VFIO container and the group
# files as uid 42427, a group only, so other users cannot open them. stackbox-vfio.service sets the
# same owner and modes at each boot; this covers the first.
if [ -e /dev/vfio/vfio ]; then chown root:42427 /dev/vfio/vfio && chmod 0660 /dev/vfio/vfio; fi
for node in /dev/vfio/[0-9]*; do [ -e "$node" ] || continue; chown root:42427 "$node" && chmod 0660 "$node"; done
systemctl daemon-reload"#,
    )
    .await?;
    if neutron {
        // Every VM that runs OVS starts its vswitchd through this hook.
        sandbox
            .fs()
            .write(OVS_EXTEND_START, OVS_EXTEND_START_SCRIPT)
            .await?;
    }
    Ok(())
}

/// Whether a release's tooling needs a Python minor the guest does not ship. Ansible of the
/// end-of-life series runs on 3.8–3.10; the guest ships 3.12.
fn needs_own_python(release: &releases::ReleaseInfo) -> bool {
    release.python != GUEST_PYTHON
}

/// The Python a release installs with: the guest's `python3` when its tooling supports it,
/// else the named minor.
fn python_command(release: &releases::ReleaseInfo) -> String {
    if needs_own_python(release) {
        format!("python{}", release.python)
    } else {
        "python3".to_owned()
    }
}

/// uv's own venv, so the system interpreter keeps no packages (Rocky enforces PEP 668).
const UV_VENV: &str = "/opt/stackbox-uv";
/// Where uv installs older minors next to the guest's own python3.
const PYTHON_DIR: &str = "/opt/stackbox-python";

/// Shell that resolves `$python` for the two venvs. A release whose tooling runs on the
/// guest's own `python3` uses it, which is always there. The end-of-life series need an older
/// minor than the guest distribution ships: a build the golden image left under /opt or
/// /usr/local is used first, and otherwise uv installs that minor into /opt.
fn python_setup(release: &releases::ReleaseInfo) -> String {
    let python = python_command(release);
    let mut setup = format!(
        r#"python={python}
if ! command -v "$python" >/dev/null; then
  for candidate in /opt/{python}/bin/{python} /usr/local/bin/{python}; do
    if [ -x "$candidate" ]; then python=$candidate; break; fi
  done
fi
"#
    );
    if !needs_own_python(release) {
        setup.push_str(
            r#"if ! command -v "$python" >/dev/null; then
  echo "this VM has no python3; install it in the golden image and start the stackbox again" >&2
  exit 1
fi
"#,
        );
        return setup.trim_end().to_owned();
    }
    let minor = &release.python;
    setup.push_str(&format!(
        r#"# The guest ships Python {guest}, which this release's Ansible is too old for.
if ! command -v "$python" >/dev/null; then
  step "Python {minor} 준비 (uv)"
  if [ ! -x {UV_VENV}/bin/uv ]; then
    python3 -m venv {UV_VENV}
    {UV_VENV}/bin/pip install -q -U pip uv
  fi
  export UV_PYTHON_INSTALL_DIR={PYTHON_DIR}
  # A retry finds the minor the first run installed and leaves it alone, so the stamp stays.
  python="$({UV_VENV}/bin/uv python find {minor} 2>/dev/null || true)"
  if [ -z "$python" ]; then
    {UV_VENV}/bin/uv python install {minor} || true
    python="$({UV_VENV}/bin/uv python find {minor} 2>/dev/null || true)"
  fi
fi
if ! command -v "$python" >/dev/null; then
  echo "this release needs Python {minor}, which uv could not install" >&2
  echo "check that the VM reaches pypi.org (uv itself) and github.com (the interpreter), then start the stackbox again" >&2
  exit 1
fi
# The stamp holds the interpreter's absolute path whatever PATH a retry starts from.
python=$(command -v "$python")
"#,
        guest = GUEST_PYTHON,
        UV_VENV = UV_VENV,
        PYTHON_DIR = PYTHON_DIR,
    ));
    setup.trim_end().to_owned()
}

/// The `pip install` line for Ansible when the release's requirements leave it out. The
/// end-of-life tags never declared it, and 2024.1's requirements.txt does not either. Each
/// quickstart installs its range into the same venv first (`doc/source/user/quickstart.rst` at
/// the tag, e.g. tag 10.4.0: "Kolla Ansible requires at least Ansible 2.8 and supports up to
/// 2.9"; 2024.1 asks for ansible-core up to 2.16).
fn ansible_install(release: &releases::ReleaseInfo) -> String {
    if release.ansible.is_empty() {
        return String::new();
    }
    if !release.eol {
        return format!(
            r#"  # This branch's requirements.txt leaves Ansible out; its quickstart installs this range.
  {VENV}/bin/pip install -q "{spec}"
"#,
            VENV = VENV,
            spec = release.ansible,
        );
    }
    format!(
        r#"  # This tag does not depend on Ansible; its series' quickstart installs this range first.
  # Ansible 2.9 and 2.10 still call Jinja2's `environmentfilter`, which 3.1 removed.
  {VENV}/bin/pip install -q "{spec}" {pins}
"#,
        VENV = VENV,
        spec = release.ansible,
        pins = PIP_PINS,
    )
}

/// What the legacy venvs additionally pin, as `pip install` arguments. Ansible 2.9 and 2.10
/// (the ranges of ussuri to xena) call Jinja2's `environmentfilter`, which 3.1 removed; the
/// pin costs nothing on the newer ranges. MarkupSafe 3.0 is newer than that Jinja2 line was
/// tested with.
const PIP_PINS: &str = r#""Jinja2<3.1" "MarkupSafe<3""#;
/// The same pins as a stamp token, so a venv built before the pin is rebuilt.
const PIP_PINS_STAMP: &str = "jinja2<3.1 markupsafe<3";

/// The dependency step: `install-deps` for a release whose launcher has it, and a note for the
/// older tags. Yoga (14.11.0) added the command for its Galaxy collections; before that the
/// roles ship in-tree and `tools/kolla-ansible` answers any other subcommand with usage (exit
/// 3), so running it there fails the phase.
fn install_deps_step(release: &releases::ReleaseInfo) -> String {
    if release.install_deps {
        return r#"step "kolla-ansible install-deps (컨테이너 런타임 의존성)"
kolla-ansible install-deps"#
            .to_owned();
    }
    format!(
        r#"# {reference} predates install-deps (added in 14.11.0): its roles are in the tree, so
# there is no Galaxy step to run.
step "kolla-ansible install-deps 생략 ({release}에는 이 서브커맨드가 없음)""#,
        reference = release.reference,
        release = release.release,
    )
}

/// The Galaxy ref `install-deps` checks out. Every tag's `requirements.yml` names
/// `stable/<series>` of `ansible-collection-kolla`, and the repository deleted that branch when
/// the series ended (`git ls-remote --tags https://opendev.org/openstack/ansible-collection-kolla`
/// lists `zed-eol` and `yoga-eol`, and neither `stable/zed` nor `stable/yoga`), so Galaxy fails
/// with "pathspec 'stable/zed' did not match any file(s) known to git". The `-eol` tag holds
/// the deleted branch's last commit. `pip` installs the file into the venv (`setup.cfg`:
/// `share/kolla-ansible = requirements.yml`) and the launcher reads it back from
/// `${VIRTUAL_ENV}/share/kolla-ansible`, so rewriting it before `install-deps` is enough. The
/// rewrite runs on every install, not only when the venv is built, so a rebuilt venv that holds
/// the tag's own file again still installs from the surviving ref.
fn galaxy_ref_step(release: &releases::ReleaseInfo) -> String {
    if release.collection.is_empty() {
        return String::new();
    }
    format!(
        r#"# {reference} names the collection branch stable/{series}, which the collection
# repository deleted at end of life; {collection} is the tag that keeps its last commit.
step "ansible-collection-kolla 참조 교체 (stable/{series} → {collection})"
{VENV}/bin/python - <<'PY'
from pathlib import Path

path = Path("{VENV}/share/kolla-ansible/requirements.yml")
text = path.read_text()
if "version: {collection}" not in text:
    marker = "version: stable/{series}"
    found = text.count(marker)
    if found != 1:
        raise RuntimeError(
            "unexpected kolla-ansible requirements.yml in %s (%d matches for %r); review the Galaxy ref for this release"
            % (path, found, marker)
        )
    path.write_text(text.replace(marker, "version: {collection}", 1))
PY
"#,
        reference = release.reference,
        series = release.series,
        collection = release.collection,
        VENV = VENV,
    )
}

/// What a venv was built from. A release that installs Ansible adds its range to the stamp (and
/// the end-of-life series its Jinja2 pin), so a venv made before they were installed is rebuilt.
fn stamp(release: &releases::ReleaseInfo) -> String {
    if release.ansible.is_empty() {
        format!("{} {} $python", release.release, release.reference)
    } else if release.eol {
        format!(
            "{} {} {} {} $python",
            release.release, release.reference, release.ansible, PIP_PINS_STAMP
        )
    } else {
        format!(
            "{} {} {} $python",
            release.release, release.reference, release.ansible
        )
    }
}

/// The `ldconfig` guard for the C library uv's standalone CPythons link. Rocky 10 drops
/// libcrypt.so.1 from glibc and keeps it in libxcrypt-compat, which the older CPythons'
/// `_crypt` needs; without it the venv's pip fails importing `crypt` mid-install.
fn libcrypt_step(release: &releases::ReleaseInfo) -> String {
    if !needs_own_python(release) {
        return String::new();
    }
    r#"# uv's CPython builds link libcrypt.so.1, which Rocky 10 keeps in libxcrypt-compat.
if ! ldconfig -p | grep 'libcrypt\.so\.1' >/dev/null; then
  step "libcrypt 호환 라이브러리 설치 (libxcrypt-compat)"
  dnf install -y -q libxcrypt-compat
fi
"#
    .to_owned()
}

/// The last line of the install. The launcher gained `--version` with the same 14.11.0 rewrite
/// that added `install-deps`; the older tags answer any option with usage, so they are only
/// checked for starting at all.
fn version_check(release: &releases::ReleaseInfo) -> String {
    if release.eol && !release.install_deps {
        format!(
            r#"step "kolla-ansible 사용 가능 확인 ({reference})"
kolla-ansible --help >/dev/null"#,
            reference = release.reference
        )
    } else {
        "step \"kolla-ansible 버전 확인\"\nkolla-ansible --version".to_owned()
    }
}

/// One change to a file in a tag's kolla-ansible tree. Every end-of-life tag needs the same
/// guest-kernel compatibility as a maintained branch, but lays its tasks out differently:
/// `master` creates each bridge with the `openvswitch_bridge` module, while the tags of ussuri
/// through zed call `kolla_ensure_openvswitch_configured`, and the `other_config` loop arrived
/// with yoga (14.11.0). Each entry therefore names the text it replaces instead of assuming
/// the maintained branch's task, and the applier fails when the file is not the reviewed one.
#[derive(serde::Serialize)]
struct LegacyPatch {
    /// What the change achieves, named when the file does not match it.
    aim: &'static str,
    /// File below `share/kolla-ansible/ansible`.
    file: &'static str,
    /// Text to replace; it must occur exactly once.
    from: &'static str,
    /// Its replacement. Already present means the patch has been applied.
    to: String,
}

/// The bridge task every end-of-life tag needs, inserted before Kolla's own "Ensuring OVS
/// bridge is properly setup". Kolla's helper (`ovs_ensure_configured.sh` in the tag's
/// openvswitch-db-server image) runs a plain `ovs-vsctl add-br`, which asks for a kernel
/// (system) datapath; the guest kernel has no openvswitch module, so the bridge would never
/// get a datapath. Creating it with `datapath_type={{ ovs_datapath }}` (netdev) leaves the
/// same state the maintained branch gets from its `openvswitch_bridge` patch. Kolla's helper
/// then finds the bridge and only adds the port.
///
/// `kolla_container_engine` first appears in the zed tag (15.6.0), where it is `docker`; the
/// older tags run docker only, so the default covers them.
const LEGACY_BRIDGE_TASK: &str = r#"- name: Create the stackbox's bridges with the userspace datapath
  become: true
  command: "{{ (kolla_container_engine | default('docker')) }} exec openvswitch_db ovs-vsctl --may-exist add-br {{ item }} -- set Bridge {{ item }} datapath_type={{ ovs_datapath }}"
  loop: "{{ neutron_bridge_name.split(',') }}"
  when:
    - inventory_hostname in groups["network"]
      or (inventory_hostname in groups["compute"] and computes_need_external_bridge | bool )

"#;

/// The `other_config` item the maintained branch's loop carries, which the ussuri to xena tags
/// do not have yet.
const LEGACY_HW_OFFLOAD_ITEM: &str = "    - { col: \"other_config\", name: \"hw-offload\", value: true, state: \"{{ 'present' if openvswitch_hw_offload | bool else 'absent' }}\" }\n";
/// The same userspace-TSO item the maintained branch's loop carries. Only compute-only VMs:
/// on the network node it breaks the router and DHCP ports and the stackbox link.
const LEGACY_TSO_LOOP_ITEM: &str = "    - { col: \"other_config\", name: \"userspace-tso-enable\", value: true, state: \"{{ 'present' if ovs_datapath == 'netdev' and inventory_hostname in groups['compute'] and inventory_hostname not in groups['network'] else 'absent' }}\" }\n";

/// The ussuri to xena "Set system-id" task, which the tags after it replaced with a
/// `kolla_toolbox` loop.
const LEGACY_SYSTEM_ID_TASK: &str = "- name: Set system-id\n  become: true\n  command: docker exec openvswitch_vswitchd ovs-vsctl set Open_vSwitch . external_ids:system-id={{ openvswitch_system_id }}\n";
/// Userspace TSO for those tags, as their own task after the system-id one.
const LEGACY_TSO_TASK: &str = r#"
- name: Enable userspace TSO on compute hosts
  become: true
  command: docker exec openvswitch_vswitchd ovs-vsctl set Open_vSwitch . other_config:userspace-tso-enable=true
  when:
    - ovs_datapath == 'netdev'
    - inventory_hostname in groups['compute']
    - inventory_hostname not in groups['network']
"#;

/// The patches an end-of-life tag takes, in the order they are applied.
fn legacy_patches(release: &releases::ReleaseInfo) -> Vec<LegacyPatch> {
    // The `other_config` loop (with the hw-offload item) arrived with yoga's tag; ussuri to
    // xena set their one item in the "Set system-id" task instead. Keyed on the series rather
    // than the ref, which a fetched branch listing can change.
    let other_config_loop = matches!(release.series.as_str(), "yoga" | "zed");
    let mut patches = vec![LegacyPatch {
        aim: "OVS bridge datapath",
        file: "roles/openvswitch/tasks/post-config.yml",
        from: "- name: Ensuring OVS bridge is properly setup\n",
        to: format!("{LEGACY_BRIDGE_TASK}- name: Ensuring OVS bridge is properly setup\n"),
    }];
    patches.push(if other_config_loop {
        LegacyPatch {
            aim: "userspace TSO",
            file: "roles/openvswitch/tasks/post-config.yml",
            from: LEGACY_HW_OFFLOAD_ITEM,
            to: format!("{LEGACY_HW_OFFLOAD_ITEM}{LEGACY_TSO_LOOP_ITEM}"),
        }
    } else {
        LegacyPatch {
            aim: "userspace TSO",
            file: "roles/openvswitch/tasks/post-config.yml",
            from: LEGACY_SYSTEM_ID_TASK,
            to: format!("{LEGACY_SYSTEM_ID_TASK}{LEGACY_TSO_TASK}"),
        }
    });
    if !other_config_loop {
        // The tags before yoga also install `drpm`, which dnf5 dropped: Rocky 10 has no such
        // package, and delta RPMs were an optimization these guests never needed.
        patches.push(LegacyPatch {
            aim: "deltarpm",
            file: "roles/baremetal/tasks/install.yml",
            from: "- name: Install deltarpm packages\n  package:\n    name: drpm\n    state: present\n    update_cache: yes\n  become: True\n  when: ansible_facts.os_family == 'RedHat'\n",
            to: "# stackbox: dnf5 dropped delta RPMs, and Rocky 10 has no drpm package.\n".into(),
        });
    }
    patches.push(LegacyPatch {
        // A control VM alone has no compute host yet; the stackbox's globals say so, and the first
        // compute joins later without redeploying the control plane.
        aim: "no-compute precheck",
        file: "roles/nova-cell/tasks/precheck.yml",
        from: "  when:\n    - groups[nova_cell_compute_group] | length < 1\n    - not nova_compute_ironic.enabled | bool\n",
        to: "  when:\n    - groups[nova_cell_compute_group] | length < 1\n    - not nova_compute_ironic.enabled | bool\n    - not (stackbox_no_computes | default(false))\n".into(),
    });
    patches.push(LegacyPatch {
        // The same kernel has no openvswitch module; stackboxes run OVS with the userspace datapath.
        aim: "netdev datapath",
        file: "roles/openvswitch/tasks/config-host.yml",
        from: "- name: Load and persist openvswitch module\n",
        to: "- name: Load and persist openvswitch module\n  when: ovs_datapath == 'system'\n"
            .into(),
    });
    patches.push(LegacyPatch {
        // Nova loads br_netfilter with modprobe; this kernel has the filter built in and no
        // modprobe. The whole task block keeps the marker unique: the file repeats the bare
        // `when` clause on the tasks that follow.
        aim: "br_netfilter",
        file: "roles/nova-cell/tasks/config-host.yml",
        from: "- name: Load and persist br_netfilter module\n  include_role:\n    name: module-load\n  vars:\n    modules:\n      - { name: br_netfilter }\n  when:\n    - inventory_hostname in groups[nova_cell_compute_group]\n",
        to: "- name: Load and persist br_netfilter module\n  include_role:\n    name: module-load\n  vars:\n    modules:\n      - { name: br_netfilter }\n  when:\n    - false\n    - inventory_hostname in groups[nova_cell_compute_group]\n".into(),
    });
    patches.push(LegacyPatch {
        // The images these tags pull print a collection-version warning in the toolbox
        // before Ansible's JSON document. The module parses the whole output, so the warning
        // fails a task that otherwise succeeded. Keep the document alone; any other preamble
        // line is dropped the same way.
        aim: "toolbox output preamble",
        file: "library/kolla_toolbox.py",
        from: "        json_output = client.exec_start(job)\n\n        try:\n            output = json.loads(json_output)\n",
        to: "        json_output = client.exec_start(job)\n        # Ansible can print warnings (an unsupported collection, for one) ahead of the JSON\n        # document; the module parses the output as a whole. Keep the document alone.\n        marker = b'{' if isinstance(json_output, bytes) else '{'\n        start = json_output.find(marker)\n        if start > 0:\n            json_output = json_output[start:]\n\n        try:\n            output = json.loads(json_output)\n".into(),
    });
    patches
}

/// Applies the guest-kernel compatibility patches to the kolla-ansible tree at `root`.
fn kernel_patches(release: &releases::ReleaseInfo, root: &str) -> String {
    if !release.eol {
        return CURRENT_KERNEL_PATCHES.replace("{VENV}", VENV);
    }
    let patches = serde_json::to_string(&legacy_patches(release)).expect("patch table");
    format!(
        r#"step "게스트 커널·툴박스 호환 패치 적용 (OVS netdev, 유저스페이스 TSO, br_netfilter)"
# This tag's task files predate the maintained branches. Every patch names the text it
# replaces, and a file that does not match fails the phase instead of silently skipping a
# change the guest needs.
python3 - <<'PY'
import json
from pathlib import Path

root = Path("{root}")
patches = json.loads(r"""{patches}""")
for patch in patches:
    path = root / patch["file"]
    text = path.read_text()
    if patch["to"] in text:
        continue
    found = text.count(patch["from"])
    if found != 1:
        raise RuntimeError(
            "unexpected Kolla task for the %s patch in %s (%d matches); review the stackbox patch for this release"
            % (patch["aim"], patch["file"], found)
        )
    path.write_text(text.replace(patch["from"], patch["to"], 1))
PY"#,
        root = root,
        patches = patches,
    )
}

/// The guest-kernel compatibility patches for a maintained branch, whose tasks these were
/// written against: its Kolla creates `br-ex` with the `openvswitch_bridge` module, loops over
/// `other_config` items and loads the openvswitch module. `{VENV}` is substituted by the
/// caller, so the YAML braces stay literal here.
const CURRENT_KERNEL_PATCHES: &str = r#"step "게스트 커널 호환 패치 적용 (OVS netdev, br_netfilter)"
# Retain the guest-kernel compatibility patches for Neutron's userspace OVS.
python3 - <<'PY'
from pathlib import Path
# Kolla creates br-ex without a datapath type, so with the netdev datapath it stays unusable
# ("No datapath_id on bridge br-ex"). `set` only applies when the bridge is first created.
path = Path("{VENV}/share/kolla-ansible/ansible/roles/openvswitch/tasks/post-config.yml")
text = path.read_text()
marker = "      fail_mode: standalone\n  loop: \"{{ neutron_bridge_name.split(',') }}\"\n"
patched = "      fail_mode: standalone\n      set: \"Bridge {{ item }} datapath_type={{ ovs_datapath }}\"\n  loop: \"{{ neutron_bridge_name.split(',') }}\"\n"
if patched not in text:
    if text.count(marker) != 1:
        raise RuntimeError("unexpected Kolla OVS bridge task; review the datapath_type patch")
    path.write_text(text.replace(marker, patched, 1))
# Virtio instances leave TCP checksums to the host. The userspace conntrack behind Neutron's OVS firewall
# checks them as they arrive and drops the replies as invalid, so only ICMP gets through. With
# userspace TSO, OVS reads the tap's offload flags instead. vswitchd applies it at start, before instances.
# Only on compute-only VMs: on the network node it breaks the router and DHCP ports and the stackbox link,
# so all-in-one stackboxes still drop instance TCP.
text = path.read_text()
marker = "    - { col: \"other_config\", name: \"hw-offload\", value: true, state: \"{{ 'present' if openvswitch_hw_offload | bool else 'absent' }}\" }\n"
patched = marker + "    - { col: \"other_config\", name: \"userspace-tso-enable\", value: true, state: \"{{ 'present' if ovs_datapath == 'netdev' and inventory_hostname in groups['compute'] and inventory_hostname not in groups['network'] else 'absent' }}\" }\n"
if patched not in text:
    if text.count(marker) != 1:
        raise RuntimeError("unexpected Kolla OVS other_config task; review the userspace TSO patch")
    path.write_text(text.replace(marker, patched, 1))
# A control VM alone has no compute host yet; the stackbox's globals say so, and the first
# compute joins later without redeploying the control plane.
path = Path("{VENV}/share/kolla-ansible/ansible/roles/nova-cell/tasks/precheck.yml")
text = path.read_text()
marker = "  when:\n    - groups[nova_cell_compute_group] | length < 1\n    - not nova_compute_ironic.enabled | bool\n"
patched = marker + "    - not (stackbox_no_computes | default(false))\n"
if patched not in text:
    if text.count(marker) != 1:
        raise RuntimeError("unexpected Kolla nova-cell precheck; review the no-compute patch")
    path.write_text(text.replace(marker, patched, 1))
# The same kernel has no openvswitch module; stackboxes run OVS with the userspace (netdev) datapath.
path = Path("{VENV}/share/kolla-ansible/ansible/roles/openvswitch/tasks/config-host.yml")
text = path.read_text()
marker = "- name: Load and persist openvswitch module\n"
guarded = marker + "  when: ovs_datapath == 'system'\n"
if guarded not in text:
    if text.count(marker) != 1:
        raise RuntimeError("unexpected Kolla openvswitch task; review the netdev datapath patch")
    path.write_text(text.replace(marker, guarded, 1))
# Nova loads br_netfilter with modprobe. This kernel has that bridge filter built in and no modprobe.
path = Path("{VENV}/share/kolla-ansible/ansible/roles/nova-cell/tasks/config-host.yml")
text = path.read_text()
marker = "  when:\n    - inventory_hostname in groups[nova_cell_compute_group]\n"
patched = "  when:\n    - false\n    - inventory_hostname in groups[nova_cell_compute_group]\n"
if text.count("- name: Load and persist br_netfilter module\n") != 1:
    raise RuntimeError("unexpected Kolla nova-cell host task; review the br_netfilter patch")
if patched not in text:
    if text.count(marker) < 1:
        raise RuntimeError("unexpected Kolla nova-cell when clause; review the br_netfilter patch")
    path.write_text(text.replace(marker, patched, 1))
PY"#;

/// Ansible 2.9 bundles six 1.12, whose `moves` importer registers itself with `find_module`,
/// which Python 3.12 no longer calls; every module payload that touches it then dies with
/// "No module named 'ansible.module_utils.six.moves'". Six 1.16 also implements `find_spec`,
/// and Ansible copies its own tree into each payload, so the venv's copy is what the guest's
/// interpreter loads. Yoga's Ansible (2.12) no longer uses six this way.
fn six_fix(release: &releases::ReleaseInfo) -> String {
    if !release.eol {
        return String::new();
    }
    format!(
        r#"# Ansible 2.9's bundled six predates Python 3.12's import hooks; the payload runs on the
# guest's interpreter, so the venv's copy of six is replaced with one that registers them.
six_target="$({VENV}/bin/python -c 'import os, ansible; print(os.path.join(os.path.dirname(ansible.__file__), "module_utils", "six", "__init__.py"))')"
if [ -e "$six_target" ]; then
  step "Ansible용 six 갱신 (게스트 Python 3.12 호환)"
  {VENV}/bin/pip install -q "six>=1.16"
  cp "$({VENV}/bin/python -c 'import six; print(six.__file__)')" "$six_target"
  rm -rf "$(dirname "$six_target")/__pycache__"
fi
"#,
        VENV = VENV
    )
}

/// The two places where the tag's Ansible knows less about this guest than its modules need.
/// Its fact map predates Rocky Linux, so a Rocky guest reports `ansible_os_family: Rocky` and
/// every `os_family == 'RedHat'` condition in the tag's bootstrap (the Docker repository and
/// its packages among them) is skipped. And Python 3.12 dropped the `cert_file`/`key_file`
/// attributes that Ansible's `CustomHTTPSConnection` still reads, which fails every module
/// that fetches a URL (`rpm_key` for the Docker key, `get_url`, ...). Both patches are no-ops
/// where the installed Ansible already handles it. Ansible builds each module payload from
/// the controller's tree, so the venv's copy is the one every guest runs.
fn ansible_fixes(release: &releases::ReleaseInfo) -> String {
    // Ansible 2.16 (2024.1 and newer) supports Python 3.12 targets; older tooling needs the
    // two no-op-when-already-fixed patches.
    if !needs_own_python(release) {
        return String::new();
    }
    format!(
        r#"# The tag's Ansible predates Rocky Linux and Python 3.12; teach it both before any play.
step "Ansible를 게스트(Rocky 10, Python 3.12)에 맞추기"
{VENV}/bin/python - <<'PY'
import os
from pathlib import Path

import ansible

root = Path(ansible.__file__).parent
# `ansible_os_family` stays "Rocky" without an entry, and Kolla gates its RedHat tasks on it.
family = root / "module_utils" / "facts" / "system" / "distribution.py"
text = family.read_text()
if "'Rocky'" not in text:
    marker = "    OS_FAMILY_MAP = {{'RedHat': ["
    if text.count(marker) != 1:
        raise RuntimeError("unexpected Ansible OS family map; review the Rocky patch")
    family.write_text(text.replace(marker, marker + "'Rocky', ", 1))
# Python 3.12 removed these from http.client; Ansible reads them on every URL fetch.
urls = root / "module_utils" / "urls.py"
text = urls.read_text()
if "self.cert_file" in text and "getattr(self, 'cert_file'" not in text:
    marker = (
        "        def __init__(self, *args, **kwargs):\n"
        "            httplib.HTTPSConnection.__init__(self, *args, **kwargs)\n"
        "            self.context = None\n"
    )
    patched = (
        "        def __init__(self, *args, **kwargs):\n"
        "            httplib.HTTPSConnection.__init__(self, *args, **kwargs)\n"
        "            # Python 3.12 removed these from http.client; the class still reads them.\n"
        "            self.cert_file = getattr(self, 'cert_file', None)\n"
        "            self.key_file = getattr(self, 'key_file', None)\n"
        "            self.context = None\n"
    )
    if text.count(marker) != 1:
        raise RuntimeError("unexpected Ansible url connection class; review the Python 3.12 patch")
    urls.write_text(text.replace(marker, patched, 1))
PY
"#,
        VENV = VENV,
    )
}

pub(crate) fn install_script(release: &releases::ReleaseInfo) -> String {
    format!(
        r#"set -euo pipefail
# Golden images install the build tools; stackboxes made from the former Docker-built image lack them.
if ! command -v gcc >/dev/null; then
  step "빌드 도구 설치 (gcc, python3-devel, libffi, openssl)"
  dnf install -y -q gcc python3-devel libffi-devel openssl-devel
fi
{libcrypt}# One venv per release: the branches differ, so a different release reinstalls it.
# {VENV}/bin joins PATH only after the interpreter is resolved. On a retry that directory
# still holds the previous venv's python, which `command -v` would then return as a bare name:
# the stamp would differ from the absolute path the first run recorded, and the script would
# delete the venv and lose the interpreter with it.
{python_setup}
stamp="{stamp}"
if [ ! -x {VENV}/bin/kolla-ansible ] || [ "$(cat {VENV}/.stackbox-release 2>/dev/null || true)" != "$stamp" ]; then
  step "kolla-ansible 가상환경 생성 ({VENV}, $python)"
  rm -rf {VENV}
  "$python" -m venv {VENV}
  {VENV}/bin/pip install -q -U pip
  step "kolla-ansible 설치 ({reference})"
  {VENV}/bin/pip install -q "git+https://opendev.org/openstack/kolla-ansible@{reference}"
{ansible_install}  printf '%s' "$stamp" > {VENV}/.stackbox-release
fi
{six_fix}{ansible_fixes}export PATH={VENV}/bin:$PATH{venv_export}
{galaxy}{install_deps}
{kernel_patches}
if [ ! -x {OSC_VENV}/bin/openstack ] || [ "$(cat {OSC_VENV}/.stackbox-release 2>/dev/null || true)" != "$stamp" ]; then
  step "OpenStack CLI 가상환경 생성 ({OSC_VENV})"
  rm -rf {OSC_VENV}
  "$python" -m venv {OSC_VENV}
  {OSC_VENV}/bin/pip install -q -U pip
  step "python-openstackclient 설치"
  {OSC_VENV}/bin/pip install -q -c https://releases.openstack.org/constraints/upper/{series} python-openstackclient
  printf '%s' "$stamp" > {OSC_VENV}/.stackbox-release
fi
{version_check}"#,
        python_setup = python_setup(release),
        libcrypt = libcrypt_step(release),
        stamp = stamp(release),
        ansible_install = ansible_install(release),
        six_fix = six_fix(release),
        ansible_fixes = ansible_fixes(release),
        install_deps = install_deps_step(release),
        galaxy = galaxy_ref_step(release),
        venv_export = if release.venv_launcher() {
            format!("\nexport VIRTUAL_ENV={VENV}")
        } else {
            String::new()
        },
        kernel_patches = kernel_patches(release, &format!("{VENV}/share/kolla-ansible/ansible")),
        version_check = version_check(release),
        reference = release.reference,
        series = release.series,
    )
}

#[allow(clippy::too_many_arguments)]
async fn configure(
    deployer: &Sandbox,
    computes: &[(Compute, Option<Sandbox>)],
    topology: &Topology,
    exposure: Option<&Exposure>,
    components: &[String],
    release: &str,
    gpu: Option<&crate::model::GpuConfig>,
    pci_devices: &[String],
    sink: &EventSink,
) -> Result<()> {
    step(sink, "passwords.yml 생성 (kolla-genpwd)")?;
    run(
        deployer,
        &format!(
            "mkdir -p {CONFIG_DIR}/config && \
             cp -n {VENV}/share/kolla-ansible/etc_examples/kolla/passwords.yml {CONFIG_DIR}/passwords.yml && \
             {VENV}/bin/kolla-genpwd -p {CONFIG_DIR}/passwords.yml"
        ),
    )
    .await?;

    // A control VM alone is already a multi-node stackbox: its computes join later, so its
    // network layout must be written the multi-node way from the first deploy. Deciding by
    // the computes present here would leave physnet1 here and physnet2 in post-deploy.
    let multinode = topology.multinode();
    if !computes.is_empty() {
        step(sink, "control VM에서 compute VM으로 SSH 키 배포")?;
        distribute_ssh_key(deployer, computes).await?;
    }
    let arch = run(deployer, "uname -m").await?;
    // Instances run where nova-compute does; a compute on another host answers over SSH.
    let probe = "if [ -c /dev/kvm ]; then echo true; else echo false; fi";
    let kvm = match computes.first() {
        None => run(deployer, probe).await?,
        Some((_, Some(sandbox))) => run(sandbox, probe).await?,
        Some((compute, None)) => {
            run(
                deployer,
                &format!(
                    "ssh -o BatchMode=yes -o ConnectTimeout=10 {} '{probe}'",
                    ssh_target(compute)?
                ),
            )
            .await?
        }
    }
    .trim()
        == "true";
    step(sink, "globals.yml 작성 (주소·서비스·게스트 제약 반영)")?;
    let template = deployer
        .fs()
        .read_to_string(&format!(
            "{VENV}/share/kolla-ansible/ansible/inventory/{}",
            if multinode { "multinode" } else { "all-in-one" }
        ))
        .await?;
    let inventory = if multinode {
        let peers: Vec<Compute> = computes.iter().map(|(c, _)| c.clone()).collect();
        let sources = compute_image_sources(deployer, computes, &releases::info(release)).await?;
        config::multinode_inventory(
            &template,
            &stackbox::hostname(stackbox::Role::Control, None),
            &peers,
            &sources,
        )?
    } else {
        template
    };
    // The Ansible controller runs on the deployer and reaches every VM at once.
    let forks = 1 + computes.len();
    let fs = deployer.fs();
    let mut globals = config::globals(
        topology.vip()?,
        &topology.interface,
        arch.trim(),
        kvm,
        components,
        multinode,
        &releases::info(release),
    );
    // Public catalog URLs retain host-side offset ports; service listeners and
    // all internal API/database calls use the VM's eth0 address and native ports.
    if let Some(exposure) = exposure {
        for c in crate::model::COMPONENTS {
            if !components.iter().any(|id| id == c.id) {
                continue;
            }
            for port in c.public {
                let Some(variable) = public_url_variable(port.id) else {
                    continue;
                };
                globals.push_str(&format!(
                    "{variable}: \"http://{}:{}\"\n",
                    exposure.host,
                    exposure.port_base + port.offset
                ));
            }
        }
    }
    // Kolla's nova-cell precheck refuses an inventory without a compute host; a control
    // VM alone is exactly that until its first compute joins.
    if multinode && computes.is_empty() {
        globals.push_str("stackbox_no_computes: true\n");
    }
    // A release whose Nova has no `mtty` support gets it from patched files stackbox mounts into
    // nova_compute (see `install_nova_mtty_files`). Kolla must mount them in the same deploy
    // that writes `enabled_mdev_types`: an unpatched nova-compute refuses that config.
    if let Some(volumes) = gpu
        .filter(|gpu| gpu.mode == crate::model::GpuMode::Mtty)
        .and_then(|_| config::nova_mtty_extra_volumes(release))
    {
        globals.push_str(&volumes);
    }
    fs.write(&format!("{CONFIG_DIR}/globals.yml"), globals)
        .await?;
    // Horizon sends users to the Compute overview after login, which answers 403 without Nova.
    let horizon_settings = format!("{CONFIG_DIR}/config/horizon/_9999-custom-settings.py");
    if components.iter().any(|c| c == "horizon") && !components.iter().any(|c| c == "nova") {
        run(deployer, &format!("mkdir -p {CONFIG_DIR}/config/horizon")).await?;
        fs.write(
            &horizon_settings,
            "# Written by stackbox: this stackbox has no Nova, so start on the image list.\nHORIZON_CONFIG[\"user_home\"] = \"/project/images/\"\n",
        )
        .await?;
    } else {
        run(deployer, &format!("rm -f {horizon_settings}")).await?;
    }
    // Skyline's login reads the public catalog entry by default, which the control VM cannot reach.
    let skyline_settings = format!("{CONFIG_DIR}/config/skyline/skyline.yaml");
    match config::skyline_override(components) {
        Some(settings) => {
            run(deployer, &format!("mkdir -p {CONFIG_DIR}/config/skyline")).await?;
            fs.write(&skyline_settings, settings).await?;
        }
        None => {
            run(deployer, &format!("rm -f {skyline_settings}")).await?;
        }
    }
    // Kolla's default hybrid iptables firewall needs physdev, which the guest kernel lacks.
    // The OVS firewall uses the userspace datapath's own conntrack instead.
    let ovs_agent = format!("{CONFIG_DIR}/config/neutron/openvswitch_agent.ini");
    let ml2 = format!("{CONFIG_DIR}/config/neutron/ml2_conf.ini");
    if components.iter().any(|c| c == "neutron") {
        run(deployer, &format!("mkdir -p {CONFIG_DIR}/config/neutron")).await?;
        if multinode {
            fs.write(&ovs_agent, config::MULTINODE_OVS_AGENT).await?;
            fs.write(&ml2, config::MULTINODE_ML2).await?;
        } else {
            fs.write(
                &ovs_agent,
                "[securitygroup]\nfirewall_driver = openvswitch\n",
            )
            .await?;
            run(deployer, &format!("rm -f {ml2}")).await?;
        }
    } else {
        run(deployer, &format!("rm -f {ovs_agent} {ml2}")).await?;
    }
    // The mediated device is the only GPU setting Kolla writes: Nova's own inventory comes
    // from it. A simulated GPU's providers are stackbox's, written through Placement after the
    // deploy (see `gpu_advertise_script`), because Kolla's provider-config file takes only
    // custom classes and this contract is the standard VGPU one.
    // Every stackbox's nova-compute resumes its guests after a restore; an mtty stackbox adds its sections.
    let nova_compute_conf = format!("{CONFIG_DIR}/config/nova/nova-compute.conf");
    let mtty = gpu.is_some_and(|gpu| gpu.mode == crate::model::GpuMode::Mtty);
    if let Some(gpu) = gpu.filter(|gpu| gpu.mode == crate::model::GpuMode::Mtty) {
        step(sink, "compute VM의 mtty mdev 장치 확인")?;
        check_mdev(deployer, computes, gpu.count).await?;
    }
    // A passthrough stackbox's devices are declared by the guest address that compute-1 shows
    // after stackbox-vfio-prepare bound them, so they are read from the VM before Nova hears of them.
    let guest_pci = if pci_devices.is_empty() {
        Vec::new()
    } else {
        step(
            sink,
            "compute-1의 PCI passthrough 장치(vfio-pci, IOMMU 그룹, VFIO 노드) 확인",
        )?;
        probe_guest_pci(deployer, computes, pci_devices.len()).await?
    };
    run(deployer, &format!("mkdir -p {CONFIG_DIR}/config/nova")).await?;
    fs.write(
        &nova_compute_conf,
        &config::nova_compute_conf(mtty, &guest_pci),
    )
    .await?;
    // The scheduler filters and the aliases every Nova service reads from nova.conf.
    let nova_global = format!("{CONFIG_DIR}/config/nova.conf");
    match config::nova_global_conf(&guest_pci) {
        Some(conf) => {
            fs.write(&nova_global, &conf).await?;
        }
        None => {
            run(deployer, &remove_managed_nova_global(&nova_global)).await?;
        }
    }
    // A GPU stackbox installs the fake nvidia-smi on every instance through Nova's static
    // vendor data: cloud-init merges this cloud-config without user-data. Images that are
    // not cloud-init (CirrOS) ignore it and use the user-data installer the API serves.
    let vendordata = format!("{CONFIG_DIR}/config/nova/vendordata.json");
    if gpu.is_some() {
        run(deployer, &format!("mkdir -p {CONFIG_DIR}/config/nova")).await?;
        fs.write(&vendordata, crate::gpu::vendordata_json()).await?;
    } else {
        run(deployer, &format!("rm -f {vendordata}")).await?;
    }
    fs.write(
        &format!("{CONFIG_DIR}/config/galera.cnf"),
        config::GALERA_CONFIG,
    )
    .await?;
    step(
        sink,
        if multinode {
            "multinode inventory와 Ansible 설정 작성"
        } else {
            "all-in-one inventory와 Ansible 설정 작성"
        },
    )?;
    fs.write(INVENTORY, inventory).await?;
    run(deployer, "mkdir -p /etc/kolla/callback_plugins").await?;
    fs.write(
        "/etc/kolla/callback_plugins/stackbox.py",
        include_str!("stackbox_callback.py"),
    )
    .await?;
    fs.write(
        "/root/.ansible.cfg",
        &format!(
            "[defaults]\nhost_key_checking = False\nforks = {forks}\nstdout_callback = stackbox\ncallback_plugins = /etc/kolla/callback_plugins\n\n[ssh_connection]\npipelining = True\n"
        ),
    )
    .await?;
    eprintln!(
        "wrote {CONFIG_DIR}/globals.yml and {INVENTORY} (vip {})",
        topology.vip()?
    );
    Ok(())
}

/// What one compute's `mtty` probe reports, as [`mdev_probe`] asks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MdevProbe {
    /// No `mtty-1` mdev type: the VM booted the stock kernel.
    Absent,
    /// The mdev type exists; `available` devices are free and `in_use` are already taken.
    Devices { available: u32, in_use: u32 },
    /// Something answered, but not with what the probe prints.
    Unreadable,
}

/// The shell side of [`MdevProbe`]. `available_instances` counts what is left, not what the
/// type can offer: an mdev already created (an instance holding one) is gone from it. The
/// number in `/sys/bus/mdev/devices` is what those instances hold. One command, one exit
/// status, so a missing type and an unreachable host stay different failures. The probe has
/// no quotes, so the compute on another host can carry it inside `ssh`'s single quotes and
/// the command substitution still runs on that compute.
const MDEV_PROBE: &str = concat!(
    "if [ -f /sys/class/mdev_bus/mtty/mdev_supported_types/mtty-1/available_instances ]; then ",
    "echo available $(cat /sys/class/mdev_bus/mtty/mdev_supported_types/mtty-1/available_instances); ",
    "echo in-use $(ls -1 /sys/bus/mdev/devices 2>/dev/null | wc -l); ",
    "else echo absent; fi",
);

fn mdev_probe(output: &str) -> MdevProbe {
    let value = |prefix: &str| {
        output
            .lines()
            .find_map(|line| line.trim().strip_prefix(prefix))
            .and_then(|value| value.trim().parse::<u32>().ok())
    };
    if output.lines().any(|line| line.trim() == "absent") {
        return MdevProbe::Absent;
    }
    match (value("available "), value("in-use ")) {
        (Some(available), Some(in_use)) => MdevProbe::Devices { available, in_use },
        _ => MdevProbe::Unreadable,
    }
}

/// Refuses an `mtty` GPU stackbox whose compute VMs boot the stock kernel: without the custom
/// libkrunfw there is no `/sys/class/mdev_bus/mtty`, so nova-compute would come up with an
/// empty VGPU inventory (or fail to create the mdev when an instance is built).
async fn check_mdev(
    deployer: &Sandbox,
    computes: &[(Compute, Option<Sandbox>)],
    count: u32,
) -> Result<()> {
    for (compute, sandbox) in computes {
        let output = match sandbox {
            Some(sandbox) => run(sandbox, MDEV_PROBE).await,
            // A compute on another host answers over the control VM's SSH key.
            None => {
                run(
                    deployer,
                    &format!(
                        "ssh -o BatchMode=yes -o ConnectTimeout=10 {} '{MDEV_PROBE}'",
                        ssh_target(compute)?
                    ),
                )
                .await
            }
        }
        .with_context(|| {
            format!(
                "cannot read {}'s mtty mediated device: the compute VM did not answer",
                compute.hostname
            )
        })?;
        match mdev_probe(&output) {
            MdevProbe::Absent => bail!(
                "{} has no mtty mediated device; the 'mtty' GPU needs compute VMs booted with stackbox's kernel (prepare the host so it installs one; see docs/libkrunfw.md and docs/gpu.md)",
                compute.hostname
            ),
            MdevProbe::Unreadable => bail!(
                "cannot read {}'s mtty device count: {}",
                compute.hostname,
                output.trim()
            ),
            MdevProbe::Devices { available, in_use } => {
                // What the type can give this stackbox is what is left plus what its own
                // instances already hold; a stackbox asked for more than that cannot boot.
                let total = available + in_use;
                if total < count {
                    bail!(
                        "{} offers {total} mtty devices ({available} free, {in_use} in use), fewer than the {count} the stackbox asks for",
                        compute.hostname
                    );
                }
            }
        }
    }
    Ok(())
}

/// The compute that receives the stackbox's PCI devices (`plan.pci_devices` reach compute-1 only).
const PCI_COMPUTE: &str = "stackbox-compute-1";

/// The command that removes the stackbox's `nova.conf` once no device needs it. A file whose first
/// line is not the stackbox's marker was written by an operator and stays.
fn remove_managed_nova_global(path: &str) -> String {
    format!(
        "if [ -f {path} ] && [ \"$(sed -n 1p {path})\" = \"{}\" ]; then rm -f {path}; fi",
        config::NOVA_GLOBAL_MARKER
    )
}

/// Runs on compute-1 once `stackbox-vfio.service` has bound the devices. Prints one `device` line
/// per `0000:00:*` function (BDF, vendor, product, driver, IOMMU group, whether the group node is
/// there), then the module parameter and the VFIO container. No single quotes: it crosses ssh.
const PCI_PROBE: &str = r#"set -eu
systemctl start stackbox-vfio.service
for dev in /sys/bus/pci/devices/0000:00:*; do
  [ -e "$dev/vendor" ] || continue
  bdf=$(basename "$dev")
  vendor=$(cut -c3- "$dev/vendor")
  product=$(cut -c3- "$dev/device")
  driver=-
  if [ -L "$dev/driver" ]; then driver=$(basename "$(readlink "$dev/driver")"); fi
  group=-
  if [ -L "$dev/iommu_group" ]; then group=$(basename "$(readlink "$dev/iommu_group")"); fi
  access=no
  if [ "$group" != - ] && [ -e "/dev/vfio/$group" ]; then access=yes; fi
  echo "device $bdf $vendor $product $driver $group $access"
done
echo "allow_unsafe_interrupts $(cat /sys/module/vfio_iommu_type1/parameters/allow_unsafe_interrupts 2>/dev/null || echo missing)"
if [ -e /dev/vfio/vfio ]; then echo "vfio_container yes"; else echo "vfio_container no"; fi
"#;

fn is_pci_id(id: &str) -> bool {
    id.len() == 4 && id.chars().all(|c| c.is_ascii_hexdigit())
}

/// Checks the compute's answer to [`PCI_PROBE`]: unsafe interrupts allowed, the VFIO container
/// present, and at least `expected` devices, each bound to `vfio-pci` with an IOMMU group whose
/// node is accessible. Returns the devices with their guest addresses.
fn parse_guest_pci_probe(output: &str, expected: usize) -> Result<Vec<GuestPciDevice>> {
    let mut devices = Vec::new();
    let mut unsafe_interrupts = "absent";
    let mut container = false;
    for line in output.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.as_slice() {
            ["device", bdf, vendor, product, driver, group, access] => {
                if !is_pci_id(vendor) || !is_pci_id(product) {
                    bail!("guest device {bdf} reports ids {vendor}:{product}, not 4-digit hex ids");
                }
                if *driver != "vfio-pci" {
                    bail!(
                        "guest device {bdf} is bound to {driver}, not vfio-pci; the passthrough cannot take it"
                    );
                }
                if *group == "-" {
                    bail!("guest device {bdf} has no IOMMU group");
                }
                let iommu_group: u32 = group
                    .parse()
                    .with_context(|| format!("guest device {bdf} has IOMMU group '{group}'"))?;
                if *access != "yes" {
                    bail!(
                        "guest device {bdf} is in IOMMU group {group}, but /dev/vfio/{group} is missing"
                    );
                }
                devices.push(GuestPciDevice {
                    bdf: bdf.to_string(),
                    vendor_id: vendor.to_string(),
                    product_id: product.to_string(),
                    iommu_group,
                });
            }
            ["device", ..] => bail!("malformed guest PCI probe line: {line}"),
            ["allow_unsafe_interrupts", value] => unsafe_interrupts = value,
            ["vfio_container", value] => container = *value == "yes",
            _ => {}
        }
    }
    if unsafe_interrupts != "Y" {
        bail!(
            "the guest's vfio_iommu_type1 allow_unsafe_interrupts is {unsafe_interrupts}, not Y; the passthrough would fail"
        );
    }
    if !container {
        bail!("the guest has no /dev/vfio/vfio container node");
    }
    if devices.len() < expected {
        bail!(
            "the guest shows {} PCI devices bound to vfio-pci, fewer than the {expected} passed through",
            devices.len()
        );
    }
    Ok(devices)
}

/// Reads compute-1's passthrough devices (see [`PCI_PROBE`]) and checks them against the stackbox's
/// `expected` count. A compute on another host answers over the control VM's SSH key.
async fn probe_guest_pci(
    deployer: &Sandbox,
    computes: &[(Compute, Option<Sandbox>)],
    expected: usize,
) -> Result<Vec<GuestPciDevice>> {
    let (compute, sandbox) = computes
        .iter()
        .find(|(compute, _)| compute.hostname == PCI_COMPUTE)
        .with_context(|| format!("the stackbox has no {PCI_COMPUTE} to pass PCI devices to"))?;
    let output = match sandbox {
        Some(sandbox) => run(sandbox, PCI_PROBE).await,
        None => {
            run(
                deployer,
                &format!(
                    "ssh -o BatchMode=yes -o ConnectTimeout=10 {} '{PCI_PROBE}'",
                    ssh_target(compute)?
                ),
            )
            .await
        }
    }
    .with_context(|| {
        format!(
            "cannot read {}'s PCI passthrough devices: the compute VM did not answer",
            compute.hostname
        )
    })?;
    parse_guest_pci_probe(&output, expected).with_context(|| {
        format!(
            "{}'s PCI passthrough devices are not ready",
            compute.hostname
        )
    })
}

/// Gives an already deployed stackbox's compute-1 its host's `vfio-pci` devices. Used by a restore
/// of a PCI-free snapshot and by an expansion that adds compute-1: the devices are read from the VM,
/// `nova.conf` and `nova-compute.conf` are written (keeping the mtty sections when `mtty`), the nova
/// roles of the control VM and compute-1 are reconfigured, and `stackbox.pci` is booted to check it.
pub async fn attach_pci(
    stackbox_id: &str,
    release: &str,
    pci_devices: &[String],
    mtty: bool,
    sink: EventSink,
) -> Result<()> {
    if pci_devices.is_empty() {
        return Ok(());
    }
    let info = releases::info(release);
    let control_name = stackbox::node_name(stackbox_id, stackbox::Role::Control, None);
    let compute_name = stackbox::node_name(stackbox_id, stackbox::Role::Compute, Some(1));
    let control = stackbox::connect(&control_name).await?;
    let mut sandboxes: Vec<Sandbox> = Vec::new();
    let result = async {
        stackbox::wait_until_booted(&control).await?;
        let compute = stackbox::connect(&compute_name).await?;
        sandboxes.push(compute.clone());
        let computes = vec![(
            Compute {
                name: compute_name.clone(),
                hostname: PCI_COMPUTE.into(),
                address: String::new(),
            },
            Some(compute),
        )];
        attach_pci_on(&control, &computes, pci_devices, mtty, &info, &sink).await
    }
    .await;
    stackbox::detach(&control).await;
    for sandbox in sandboxes {
        stackbox::detach(&sandbox).await;
    }
    result
}

/// The PCI half of [`attach_pci`] on a deployer that already reaches compute-1 (`computes`).
async fn attach_pci_on(
    deployer: &Sandbox,
    computes: &[(Compute, Option<Sandbox>)],
    pci_devices: &[String],
    mtty: bool,
    release: &releases::ReleaseInfo,
    sink: &EventSink,
) -> Result<()> {
    step(
        sink,
        "compute-1의 PCI passthrough 장치(vfio-pci, IOMMU 그룹, VFIO 노드) 확인",
    )?;
    let guest = probe_guest_pci(deployer, computes, pci_devices.len()).await?;
    step(
        sink,
        "nova.conf·nova-compute.conf에 PCI alias와 device_spec 기록",
    )?;
    run(deployer, &format!("mkdir -p {CONFIG_DIR}/config/nova")).await?;
    let fs = deployer.fs();
    fs.write(
        &format!("{CONFIG_DIR}/config/nova/nova-compute.conf"),
        &config::nova_compute_conf(mtty, &guest),
    )
    .await?;
    let nova_global = format!("{CONFIG_DIR}/config/nova.conf");
    match config::nova_global_conf(&guest) {
        Some(conf) => fs.write(&nova_global, &conf).await?,
        None => {
            run(deployer, &remove_managed_nova_global(&nova_global)).await?;
        }
    }
    step(
        sink,
        format!("kolla-ansible reconfigure --tags nova (control VM, {PCI_COMPUTE})"),
    )?;
    stream(
        deployer,
        &kolla_ansible_nova_reconfigure(release),
        &format!("{LOG_DIR}/pci-reconfigure.log"),
        sink,
    )
    .await?;
    step(
        sink,
        "PCI passthrough 확인 (compute-1 장치와 stackbox.pci 인스턴스)",
    )?;
    let script = format!(
        "set -euo pipefail\n. {CONFIG_DIR}/admin-openrc.sh\nos={OSC_VENV}/bin/openstack\ncleanup() {{ :; }}\n{}",
        pci_verify_script(PCI_COMPUTE, &guest)
    );
    stream(
        deployer,
        &script,
        &format!("{LOG_DIR}/pci-verify.log"),
        sink,
    )
    .await
}

/// `reconfigure --tags nova` limited to the control VM and compute-1. The control VM's nova-api and
/// nova-scheduler need the `[pci]` alias and `PciPassthroughFilter` as much as compute-1 does.
fn kolla_ansible_nova_reconfigure(release: &releases::ReleaseInfo) -> String {
    format!(
        "{env}PATH={VENV}/bin:$PATH kolla-ansible reconfigure -i {INVENTORY} --limit stackbox-control,{PCI_COMPUTE} --tags nova",
        env = venv_env(release)
    )
}

/// Builds the patched Nova files on every host that runs nova_compute: the deployer when the
/// stackbox is all-in-one, else each compute. A compute on another host is reached over the
/// control VM's SSH key, as [`check_mdev`] reaches it.
async fn install_nova_mtty_files(
    deployer: &Sandbox,
    multinode: bool,
    computes: &[(Compute, Option<Sandbox>)],
    release: &str,
) -> Result<()> {
    let script = config::nova_mtty_prepare_script(release)
        .with_context(|| format!("{release} needs no mtty backport"))?;
    if !multinode {
        return prepare_nova_mtty(deployer, &script).await;
    }
    let prepare = config::NOVA_MTTY_PREPARE;
    deployer.fs().write(prepare, &script).await?;
    for (compute, sandbox) in computes {
        match sandbox {
            Some(sandbox) => prepare_nova_mtty(sandbox, &script)
                .await
                .with_context(|| format!("preparing {}'s mtty backport files", compute.hostname))?,
            None => {
                let host = ssh_target(compute)?;
                run(
                    deployer,
                    &format!(
                        "ssh -o BatchMode=yes -o ConnectTimeout=10 {host} 'mkdir -p /etc/stackbox && cat > {prepare}' < {prepare} && ssh -o BatchMode=yes -o ConnectTimeout=10 {host} 'bash {prepare}' < /dev/null"
                    ),
                )
                .await
                .with_context(|| format!("preparing {}'s mtty backport files", compute.hostname))?;
            }
        }
    }
    Ok(())
}

/// Puts the preparation script on `sandbox` and runs it.
async fn prepare_nova_mtty(sandbox: &Sandbox, script: &str) -> Result<()> {
    let prepare = config::NOVA_MTTY_PREPARE;
    run(sandbox, "mkdir -p /etc/stackbox").await?;
    sandbox.fs().write(prepare, script).await?;
    run(sandbox, &format!("bash {prepare}")).await?;
    Ok(())
}

/// Whether nova_compute gets the `mtty` backport: an `mtty` GPU on a release whose Nova
/// lacks `mtty` support.
fn nova_mtty_backport(gpu: Option<&crate::model::GpuConfig>, release: &str) -> bool {
    gpu.is_some_and(|gpu| gpu.mode == crate::model::GpuMode::Mtty)
        && config::nova_mtty_patch(release).is_some()
}

/// A compute's name as it travels in a shell word to the control VM's `ssh`. Stackbox builds
/// these names, but a name that is not a lowercase hostname must not reach a command line.
fn ssh_target(compute: &Compute) -> Result<&str> {
    let hostname = compute.hostname.as_str();
    anyhow::ensure!(
        hostname.starts_with("stackbox-")
            && hostname
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "unexpected compute hostname '{hostname}'"
    );
    Ok(hostname)
}

/// The Kolla image source (tag suffix and registry) of each compute, by hostname, from the
/// architecture the compute reports. A compute on another host may differ from the control VM;
/// it is refused when the release publishes no images for its architecture. Needs the control
/// VM's SSH key on the computes it reaches over SSH.
async fn compute_image_sources(
    deployer: &Sandbox,
    computes: &[(Compute, Option<Sandbox>)],
    release: &releases::ReleaseInfo,
) -> Result<BTreeMap<String, config::ImageSource>> {
    let mut sources = BTreeMap::new();
    for (compute, sandbox) in computes {
        let arch = match sandbox {
            Some(sandbox) => run(sandbox, "uname -m").await?,
            None => {
                run(
                    deployer,
                    &format!(
                        "ssh -o BatchMode=yes -o ConnectTimeout=10 {} 'uname -m'",
                        ssh_target(compute)?
                    ),
                )
                .await?
            }
        };
        let arch = arch.trim();
        anyhow::ensure!(
            images_published_for(arch, release),
            "{} is {arch}, but OpenStack {} publishes no Kolla images for {arch}",
            compute.hostname,
            release.release
        );
        sources.insert(
            compute.hostname.clone(),
            config::image_source(arch, release),
        );
    }
    Ok(sources)
}

/// The architecture compute-1 reports, which the verification instance boots on; `None` for
/// an all-in-one VM, which runs it itself. A compute on another host answers over SSH. Only a
/// plain machine name reaches the script.
async fn instance_arch(
    deployer: &Sandbox,
    computes: &[(Compute, Option<Sandbox>)],
) -> Result<Option<String>> {
    let arch = match computes.first() {
        None => return Ok(None),
        Some((_, Some(sandbox))) => run(sandbox, "uname -m").await?,
        Some((compute, None)) => {
            run(
                deployer,
                &format!(
                    "ssh -o BatchMode=yes -o ConnectTimeout=10 {} 'uname -m'",
                    ssh_target(compute)?
                ),
            )
            .await?
        }
    };
    let arch = arch.trim();
    anyhow::ensure!(
        !arch.is_empty() && arch.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "unexpected compute architecture '{arch}'"
    );
    Ok(Some(arch.to_owned()))
}

/// Whether `release`'s Kolla images exist for a VM of `arch`, as `uname -m` names it.
fn images_published_for(arch: &str, release: &releases::ReleaseInfo) -> bool {
    match arch {
        "x86_64" => true,
        "aarch64" => release.arm64,
        _ => false,
    }
}

/// Let Kolla on the control VM reach every compute as root over the management link.
async fn distribute_ssh_key(
    deployer: &Sandbox,
    computes: &[(Compute, Option<Sandbox>)],
) -> Result<()> {
    let key = run(
        deployer,
        "[ -f /root/.ssh/id_ed25519 ] || ssh-keygen -q -t ed25519 -N '' -f /root/.ssh/id_ed25519; cat /root/.ssh/id_ed25519.pub",
    )
    .await?;
    let key = key.trim();
    anyhow::ensure!(
        key.starts_with("ssh-ed25519 ") && !key.contains(['\'', '\n']),
        "unexpected SSH public key on the control VM"
    );
    for (compute, sandbox) in computes {
        // A compute on another host got the key when it was prepared there.
        if let Some(sandbox) = sandbox {
            authorize_key(sandbox, key).await?;
        }
        run(
            deployer,
            &format!(
                "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 {} true",
                ssh_target(compute)?,
            ),
        )
        .await
        .with_context(|| format!("SSH from the control VM to {}", compute.hostname))?;
    }
    Ok(())
}

/// Let `key` in as root.
async fn authorize_key(sandbox: &Sandbox, key: &str) -> Result<()> {
    anyhow::ensure!(
        key.starts_with("ssh-ed25519 ") && !key.contains(['\'', '\n']),
        "unexpected SSH public key"
    );
    // An authorized_keys without a final newline would join the key to its last line.
    run(
        sandbox,
        &format!(
            r#"set -e
mkdir -p /root/.ssh && chmod 700 /root/.ssh
f=/root/.ssh/authorized_keys
touch "$f" && chmod 600 "$f"
grep -xF '{key}' "$f" >/dev/null || {{ [ -s "$f" ] && [ -n "$(tail -c1 "$f")" ] && echo >> "$f"; echo '{key}' >> "$f"; }}"#
        ),
    )
    .await?;
    Ok(())
}

/// Wait until a deployed stackbox's services answer again after its VM started from disk.
/// With `rebind`, first point the public catalog entries recorded for `from` at `to`:
/// a stackbox restored from a snapshot publishes the services on its own host ports.
pub async fn settle(
    stackbox_id: &str,
    rebind: Option<(&Exposure, &Exposure)>,
    components: &[String],
    sink: EventSink,
) -> Result<()> {
    let name = stackbox::all_in_one_name(stackbox_id).await?;
    let sandbox = stackbox::connect(&name).await?;
    let result = async {
        stackbox::wait_until_booted(&sandbox).await?;
        stream(
            &sandbox,
            &settle_script(rebind, components),
            &format!("{LOG_DIR}/settle.log"),
            &sink,
        )
        .await
    }
    .await;
    stackbox::detach(&sandbox).await;
    result
}

/// Public base URL prefixes of `from`, keyed to the same services on `to`.
fn rebind_map(from: &Exposure, to: &Exposure) -> BTreeMap<String, String> {
    crate::model::COMPONENTS
        .iter()
        .flat_map(|c| c.public.iter())
        .map(|p| {
            (
                format!("http://{}:{}", from.host, from.port_base + p.offset),
                format!("http://{}:{}", to.host, to.port_base + p.offset),
            )
        })
        .filter(|(old, new)| old != new)
        .collect()
}

fn settle_script(rebind: Option<(&Exposure, &Exposure)>, components: &[String]) -> String {
    let has = |id: &str| components.iter().any(|c| c == id);
    let mut script = format!(
        r#"set -euo pipefail
. {CONFIG_DIR}/admin-openrc.sh
os={OSC_VENV}/bin/openstack
ok=''
for _ in $(seq 120); do
  if $os token issue -f value -c expires >/dev/null 2>&1; then ok=1; break; fi
  sleep 5
done
[ -n "$ok" ] || {{ echo "keystone did not answer within 10 minutes" >&2; exit 1; }}
echo "keystone OK"
"#
    );
    let mapping = rebind
        .map(|(from, to)| rebind_map(from, to))
        .unwrap_or_default();
    if !mapping.is_empty() {
        // Replace only whole host:port prefixes, keeping each URL's scheme and version path.
        script.push_str(&format!(
            r#"python3 - <<'PY'
import json, subprocess
from pathlib import Path
os_ = "{OSC_VENV}/bin/openstack"
mapping = json.loads(r'''{mapping}''')
def moved(url):
    for old, new in mapping.items():
        if url == old or url.startswith(old + "/"):
            return new + url[len(old):]
for ep in json.loads(subprocess.check_output([os_, "endpoint", "list", "--interface", "public", "-f", "json"])):
    url = moved(ep["URL"])
    if url:
        subprocess.check_call([os_, "endpoint", "set", "--url", url, ep["ID"]])
        print(f"{{ep['Service Name']}} public endpoint: {{ep['URL']}} -> {{url}}", flush=True)
# Keep globals.yml in step so a later redeploy registers the same URLs.
path = Path("{CONFIG_DIR}/globals.yml")
text = path.read_text()
for old, new in mapping.items():
    text = text.replace(f'"{{old}}"', f'"{{new}}"')
path.write_text(text)
PY
"#,
            mapping = serde_json::to_string(&mapping).expect("string map serializes"),
        ));
    }
    script.push_str(
        r#"for _ in $(seq 60); do $os image list -f value -c ID >/dev/null 2>&1 && break; sleep 5; done
$os image list -f value -c ID >/dev/null
echo "glance OK"
"#,
    );
    if has("neutron") {
        script.push_str(
            r#"agents=''
for _ in $(seq 100); do
  agents=$($os network agent list -f value -c "Agent Type" -c Alive 2>/dev/null || true)
  if [ "$(echo "$agents" | grep -c True)" -ge 4 ] && ! echo "$agents" | grep False >/dev/null; then break; fi
  sleep 3
done
echo "$agents"
for agent in "Open vSwitch agent" "DHCP agent" "L3 agent" "Metadata agent"; do
  echo "$agents" | grep "^$agent True" >/dev/null || { echo "$agent is not alive" >&2; exit 1; }
done
echo "neutron OK"
"#,
        );
    }
    if has("nova") {
        script.push_str(&format!(
            r#"compute_state=''
# A stackbox restored from a control template has no compute VM, so no nova-compute, until one
# is added; Kolla's multinode inventory then lists no compute host.
if {no_computes}; then
  $os compute service list -f value -c ID >/dev/null
  echo "nova OK (no compute VM yet)"
else
# Every nova-compute, one per compute VM of a multi-node stackbox.
for _ in $(seq 60); do
  compute_state=$($os compute service list --service nova-compute -f value -c State 2>/dev/null || true)
  [ -n "$compute_state" ] && ! echo "$compute_state" | grep -vx up >/dev/null && break
  sleep 5
done
[ -n "$compute_state" ] && ! echo "$compute_state" | grep -vx up >/dev/null || {{ echo "nova-compute is not up everywhere: ${{compute_state:-missing}}" >&2; exit 1; }}
echo "nova OK"
fi
"#,
            no_computes = no_compute_hosts(INVENTORY),
        ));
    }
    script.push_str("echo \"services answer\"\n");
    script
}

/// A shell test that Kolla's multinode inventory at `inventory` lists no compute host; an
/// all-in-one inventory lists the deployer there.
fn no_compute_hosts(inventory: &str) -> String {
    format!(
        r#"[ -f {inventory} ] && grep -x '\[compute\]' {inventory} >/dev/null && [ "$(awk '/^\[/ {{ group = $0; next }} group == "[compute]" && NF && !/^#/ {{ n++ }} END {{ print n + 0 }}' {inventory})" = 0 ]"#
    )
}

/// Kolla variable holding each published port's public base URL (without the API version path),
/// keyed by [`crate::model::PublicPort::id`]. The 2025.1 names are those of the 2025.2, 2026.1
/// and 2026.2 kolla-ansible branches.
fn public_url_variable(port: &str) -> Option<&'static str> {
    Some(match port {
        "keystone" => "keystone_public_url",
        "glance" => "glance_public_endpoint",
        "placement" => "placement_public_endpoint",
        "horizon" => "horizon_public_endpoint",
        "neutron" => "neutron_public_endpoint",
        "nova" => "nova_public_base_endpoint",
        "heat" => "heat_public_base_endpoint",
        "heat-cfn" => "heat_cfn_public_base_endpoint",
        "barbican" => "barbican_public_endpoint",
        "designate" => "designate_public_endpoint",
        "magnum" => "magnum_public_base_endpoint",
        "skyline" => "skyline_apiserver_public_endpoint",
        "skyline-console" => "skyline_console_public_endpoint",
        _ => return None,
    })
}

/// `multinode`: the external network's bridge is the control VM's second one (physnet2);
/// physnet1 carries the tenant VLANs.
fn post_deploy_script(
    components: &[String],
    multinode: bool,
    release: &releases::ReleaseInfo,
) -> String {
    let physnet = if multinode { "physnet2" } else { "physnet1" };
    let mut script = kolla_ansible("post-deploy", release);
    // The portal's Shell tab runs a login shell: give it the CLI and admin credentials.
    script.push_str(&format!(
        r#"
cat > /etc/profile.d/openstack.sh <<'EOF'
export PATH={OSC_VENV}/bin:$PATH
if [ -r {CONFIG_DIR}/admin-openrc.sh ]; then . {CONFIG_DIR}/admin-openrc.sh; fi
EOF"#
    ));
    if components.iter().any(|c| c == "neutron") {
        // A shared external network for routers and floating IPs, created once per stackbox.
        script.push_str(&format!(
            r#"
set -euo pipefail
. {CONFIG_DIR}/admin-openrc.sh
os={OSC_VENV}/bin/openstack
step "외부 네트워크 public 구성 (flat, {physnet})"
if ! $os network show public >/dev/null 2>&1; then
  $os network create --external --share --provider-network-type flat --provider-physical-network {physnet} public
fi
if ! $os subnet show public-subnet >/dev/null 2>&1; then
  $os subnet create --network public --subnet-range {EXTERNAL_NETWORK} --gateway {EXTERNAL_GATEWAY} \
    --allocation-pool start=172.24.4.10,end=172.24.4.250 --no-dhcp public-subnet
fi
echo "external network public ready on {EXTERNAL_NETWORK}""#
        ));
    }
    script
}

/// Kolla's Libvirt version check delegates to the first compute host, and ansible-core
/// templates `delegate_to` before the block's `when`, so with no compute VM yet it fails
/// on an empty group. `add-compute` deploys the compute VM later, which runs the check.
fn deploy_script(release: &releases::ReleaseInfo) -> String {
    let deploy = kolla_ansible("deploy", release);
    format!(
        "if {no_computes}; then {deploy} --skip-tags nova-libvirt-version-check; else {deploy}; fi",
        no_computes = no_compute_hosts(INVENTORY),
    )
}

fn kolla_ansible(command: &str, release: &releases::ReleaseInfo) -> String {
    // kolla-ansible shells out to ansible-playbook and ansible-galaxy from PATH.
    format!(
        "{env}PATH={VENV}/bin:$PATH kolla-ansible {command} -i {INVENTORY}",
        env = venv_env(release)
    )
}

/// `computes`: the nova-compute services that must be up; none for a control VM whose
/// computes join later, which only checks that the Nova API answers.
///
/// `release` names the OpenStack series the stackbox installed; it decides whether a `mtty`
/// stackbox's nova-compute carries stackbox's backport (see [`config::nova_mtty_patch`]).
fn verify_script(
    components: &[String],
    computes: usize,
    release: &str,
    gpu: Option<&crate::model::GpuConfig>,
) -> String {
    let has = |id: &str| components.iter().any(|c| c == id);
    let mut script = format!(
        r#"set -euo pipefail
. {CONFIG_DIR}/admin-openrc.sh
os={OSC_VENV}/bin/openstack
step "Keystone token 발급"
$os token issue -f value -c expires
# Old clients match the display header (`Interface`), newer ones the API field.
$os endpoint list --service glance -f value -c Interface -c interface -c URL -c url
work=$(mktemp -d)
image_id=''
extra_cleanup() {{ :; }}
cleanup() {{
  if [ -n "$image_id" ]; then $os image delete "$image_id"; fi
  extra_cleanup || true
  rm -rf "$work"
}}
trap cleanup EXIT
step "Glance 이미지 업로드·다운로드 검증"
head -c 1048576 /dev/urandom > "$work/upload.img"
image_id=$($os image create --disk-format raw --container-format bare --file "$work/upload.img" "stackbox-verify-$(cat /proc/sys/kernel/random/uuid)" -f value -c id)
test "$($os image show "$image_id" -f value -c status)" = active
$os image save --file "$work/download.img" "$image_id"
cmp "$work/upload.img" "$work/download.img"
$os image delete "$image_id"
image_id=''
echo "keystone and glance OK"
"#
    );
    if has("placement") {
        script.push_str(
            r#"step "Placement resource_providers 조회"
token=$($os token issue -f value -c id)
placement=$($os endpoint list --service placement --interface internal -f value -c URL -c url)
curl -sSf -o /dev/null -H "X-Auth-Token: $token" -H "OpenStack-API-Version: placement 1.0" "$placement/resource_providers"
echo "placement OK"
"#,
        );
    }
    if has("horizon") {
        // Log in directly like a browser: exercise Django, Keystone and memcached sessions.
        script.push_str(
            r#"step "Horizon 로그인과 대시보드 확인"
horizon="http://$(awk -F'"' '/^kolla_internal_vip_address:/ {print $2}' /etc/kolla/globals.yml)"
jar="$work/horizon.cookies"
curl -sSf -c "$jar" -o "$work/login.html" "$horizon/auth/login/"
csrf=$(awk '$6 == "csrftoken" {print $7}' "$jar")
test -n "$csrf"
region=$(sed -n 's/.*name="region" value="\([^"]*\)".*/\1/p' "$work/login.html" | awk 'NR == 1')
status=$(curl -sS -b "$jar" -c "$jar" -o /dev/null -w '%{http_code} %{redirect_url}' \
  -H "Referer: $horizon/auth/login/" \
  --data-urlencode "csrfmiddlewaretoken=$csrf" \
  --data-urlencode "username=$OS_USERNAME" \
  --data-urlencode "password=$OS_PASSWORD" \
  --data-urlencode "domain=$OS_USER_DOMAIN_NAME" \
  --data-urlencode "region=$region" \
  --data-urlencode "next=/" \
  "$horizon/auth/login/")
case "$status" in
  302*/auth/login/*) echo "horizon rejected the admin login" >&2; exit 1 ;;
  302*) ;;
  *) echo "horizon login returned $status" >&2; exit 1 ;;
esac
home=$(curl -sS -L -b "$jar" -o /dev/null -w '%{http_code} %{url_effective}' "$horizon/")
case "$home" in
  "200 "*/auth/login/*|"200 ") echo "horizon dashboard returned $home" >&2; exit 1 ;;
  "200 "*) ;;
  *) echo "horizon dashboard returned $home" >&2; exit 1 ;;
esac
echo "horizon OK: ${home#200 }"
"#,
        );
    }
    if has("skyline") {
        script.push_str(&skyline_verify_script());
    }
    if has("neutron") {
        script.push_str(&neutron_verify_script());
    }
    if has("nova") {
        if computes == 0 {
            script.push_str(
                "step \"Nova API 확인 (compute VM 없음)\"\n$os compute service list -f value -c ID >/dev/null\necho \"nova OK (no compute VM yet)\"\n",
            );
        } else {
            script.push_str(&nova_verify_script(computes));
        }
        // A GPU stackbox advertises its providers first, then verifies them with a flavor and
        // the allocation the instance it booted holds.
        if let Some(gpu) = gpu
            && computes > 0
        {
            let hosts: Vec<String> = (1..=computes)
                .map(|index| format!("stackbox-compute-{index}"))
                .collect();
            let advertise = gpu_advertise_script(Some(gpu), &hosts);
            if !advertise.is_empty() {
                script.push_str(&advertise);
            }
            script.push_str(&gpu_verify_script(gpu, computes, release));
        }
    }
    script
}

/// Builds a throwaway tenant network behind a router on `public`, then proves the
/// userspace datapath forwards: the all-in-one host pings the router's gateway and,
/// from the router namespace, the DHCP port on the tenant network.
/// Skyline's API answers the login the browser uses, on the internal address (port 9998, the
/// apiserver's default). Its login response carries a Keystone token, as kolla's own test checks.
fn skyline_verify_script() -> String {
    r#"step "Skyline 로그인 확인"
skyline="http://$(awk -F'"' '/^kolla_internal_vip_address:/ {print $2}' /etc/kolla/globals.yml):9998"
status=$(curl -sS -o "$work/skyline.json" -w '%{http_code}' \
  -H 'Accept: application/json' -H 'Content-Type: application/json' \
  -d "{\"region\": \"$OS_REGION_NAME\", \"domain\": \"Default\", \"username\": \"admin\", \"password\": \"$OS_PASSWORD\"}" \
  "$skyline/api/v1/login") || true
if [ "$status" != 200 ]; then
  echo "skyline login returned $status ($skyline/api/v1/login)" >&2
  exit 1
fi
grep '"keystone_token"' "$work/skyline.json" >/dev/null || { echo "skyline login response has no keystone_token" >&2; exit 1; }
echo "skyline OK: $skyline"
"#
    .to_owned()
}

/// The failure message for `phase`. Phases after `configure` read the configuration that
/// `configure` wrote, so a user who edited it must resume from `configure` to rewrite it.
fn resume_hint(stackbox_id: &str, phase: Phase) -> String {
    let mut hint = format!(
        "phase '{phase}' failed; resume with `stackbox deploy {stackbox_id} --from {phase}`"
    );
    if phase > Phase::Configure {
        hint.push_str(&format!(
            "; if you changed the configuration, resume with `stackbox deploy {stackbox_id} --from configure`"
        ));
    }
    hint
}

fn neutron_verify_script() -> String {
    r#"step "Neutron agent 상태 확인 (OVS·DHCP·L3·Metadata)"
for _ in $(seq 60); do
  agents=$($os network agent list -f value -c "Agent Type" -c Alive)
  if [ "$(echo "$agents" | grep -c True)" -ge 4 ] && ! echo "$agents" | grep False >/dev/null; then break; fi
  sleep 3
done
echo "$agents"
for agent in "Open vSwitch agent" "DHCP agent" "L3 agent" "Metadata agent"; do
  echo "$agents" | grep "^$agent True" >/dev/null || { echo "$agent is not alive" >&2; exit 1; }
done
step "테넌트 네트워크·라우터·DHCP 포트 검증"
name="stackbox-verify-$(cut -c1-8 /proc/sys/kernel/random/uuid)"
net_id=''; router_id=''; subnet_id=''
extra_cleanup() {
  if [ -n "$router_id" ]; then
    if [ -n "$subnet_id" ]; then $os router remove subnet "$router_id" "$subnet_id" || true; fi
    $os router delete "$router_id" || true
  fi
  if [ -n "$net_id" ]; then $os network delete "$net_id" || true; fi
}
net_id=$($os network create "$name" -f value -c id)
subnet_id=$($os subnet create --network "$net_id" --subnet-range 10.199.0.0/24 "$name" -f value -c id)
router_id=$($os router create --external-gateway public "$name" -f value -c id)
$os router add subnet "$router_id" "$subnet_id"
gateway=$($os router show "$router_id" -f json -c external_gateway_info | python3 -c 'import json,sys; print(json.load(sys.stdin)["external_gateway_info"]["external_fixed_ips"][0]["ip_address"])')
dhcp=''
for _ in $(seq 40); do
  dhcp=$($os port list --network "$net_id" --device-owner network:dhcp -f json -c "Fixed IP Addresses" -c Status | python3 -c 'import json,sys; p=[x for x in json.load(sys.stdin) if x["Status"]=="ACTIVE"]; print(p[0]["Fixed IP Addresses"][0]["ip_address"] if p else "")')
  [ -n "$dhcp" ] && break
  sleep 3
done
test -n "$dhcp" || { echo "DHCP port did not become active" >&2; exit 1; }
for _ in $(seq 20); do ping -c1 -W2 "$gateway" >/dev/null && break; sleep 3; done
ping -c3 -W2 "$gateway"
ip netns exec "qrouter-$router_id" ping -c3 -W2 "$dhcp"
extra_cleanup
router_id=''; net_id=''
echo "neutron OK: router gateway $gateway and DHCP port $dhcp reachable"
"#
    .to_owned()
}

/// Waits for `computes` nova-compute services, then boots a tiny Cirros instance and pings
/// it from the router namespace.
fn nova_verify_script(computes: usize) -> String {
    format!(
        r#"step "nova-compute 서비스 상태 확인 ({computes}개)"
for _ in $(seq 40); do
  compute_state=$($os compute service list --service nova-compute -f value -c State)
  [ "$(echo "$compute_state" | grep -cx up)" -ge {computes} ] && break
  sleep 5
done
[ "$(echo "$compute_state" | grep -cx up)" -ge {computes} ] || {{ echo "nova-compute is not up on {computes} hosts: ${{compute_state:-missing}}" >&2; exit 1; }}
"#
    ) + r#"
$os flavor show m1.tiny >/dev/null 2>&1 || $os flavor create --ram 512 --disk 1 --vcpus 1 m1.tiny >/dev/null
step "Cirros 인스턴스 생성과 라우터 경유 ping 검증"
# Other projects get their own "default" group once Neutron sees them, so the name alone is ambiguous.
sg=$($os security group list --project "$($os token issue -f value -c project_id)" -f value -c ID -c Name | awk '$2 == "default" {print $1}')
$os security group rule create --ingress --protocol icmp --ethertype IPv4 "$sg" >/dev/null 2>&1 || true
$os security group rule create --ingress --protocol tcp --dst-port 22 --ethertype IPv4 "$sg" >/dev/null 2>&1 || true
# The instance runs on a compute, whose architecture may differ from this control VM's:
# the deploy names compute-1's as instance_arch. Glance's hw_architecture keeps the instance
# off computes of another one.
arch=${instance_arch:-$(uname -m)}
case "$arch" in
  aarch64|arm64)
    arch=aarch64
    image_url=https://download.cirros-cloud.net/0.6.3/cirros-0.6.3-aarch64-disk.img
    image_props='--property hw_firmware_type=uefi --property hw_machine_type=virt'
    ;;
  *)
    image_url=https://download.cirros-cloud.net/0.6.3/cirros-0.6.3-x86_64-disk.img
    image_props=''
    ;;
esac
image_props="$image_props --property hw_architecture=$arch"
image_name=cirros-stackbox
if ! $os image show "$image_name" >/dev/null 2>&1; then
  curl -fsSL -o "$work/cirros.img" "$image_url"
  # shellcheck disable=SC2086
  $os image create --disk-format qcow2 --container-format bare --property os_distro=cirros $image_props --file "$work/cirros.img" "$image_name" >/dev/null
fi
name="stackbox-vm-$(cut -c1-8 /proc/sys/kernel/random/uuid)"
net_id=''; router_id=''; subnet_id=''; server_id=''
extra_cleanup() {
  if [ -n "$server_id" ]; then $os server delete "$server_id" >/dev/null 2>&1 || true; fi
  if [ -n "$router_id" ]; then
    if [ -n "$subnet_id" ]; then $os router remove subnet "$router_id" "$subnet_id" >/dev/null 2>&1 || true; fi
    $os router delete "$router_id" >/dev/null 2>&1 || true
  fi
  if [ -n "$net_id" ]; then $os network delete "$net_id" >/dev/null 2>&1 || true; fi
}
net_id=$($os network create "$name" -f value -c id)
subnet_id=$($os subnet create --network "$net_id" --subnet-range 10.198.0.0/24 "$name" -f value -c id)
router_id=$($os router create --external-gateway public "$name" -f value -c id)
$os router add subnet "$router_id" "$subnet_id" >/dev/null
server_id=$($os server create --flavor m1.tiny --image "$image_name" --network "$net_id" "$name" -f value -c id)
status=''
for _ in $(seq 90); do
  status=$($os server show "$server_id" -f value -c status)
  case "$status" in
    ACTIVE) break ;;
    ERROR) echo "instance entered ERROR" >&2; $os server show "$server_id" -f value -c fault >&2; exit 1 ;;
  esac
  sleep 10
done
[ "$status" = ACTIVE ] || { echo "instance status: ${status:-missing}" >&2; exit 1; }
fixed=$($os port list --server "$server_id" -f json -c "Fixed IP Addresses" | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["Fixed IP Addresses"][0]["ip_address"])')
for _ in $(seq 60); do
  ip netns exec "qrouter-$router_id" ping -c1 -W2 "$fixed" >/dev/null && break
  sleep 10
done
ip netns exec "qrouter-$router_id" ping -c3 -W2 "$fixed"
# ICMP passes even when the OVS firewall drops TCP as invalid, so also open Cirros's SSH port.
tcp=''
for _ in $(seq 30); do
  if ip netns exec "qrouter-$router_id" timeout 3 bash -c "echo > /dev/tcp/$fixed/22" 2>/dev/null; then tcp=ok; break; fi
  sleep 5
done
[ -n "$tcp" ] || { echo "instance answered ping but not TCP port 22" >&2; exit 1; }
extra_cleanup
server_id=''; router_id=''; net_id=''
echo "nova OK: $name at $fixed answered ping and TCP from the router"
"#
}

/// The child resource providers a `simulated` GPU stackbox advertises, one per fake GPU of each
/// of `hosts`: the name, parent and inventory a vGPU deployment's Nova gives its own, so a
/// flavor that names the standard class and a custom trait schedules here.
///
/// Nothing else writes them. Nova's libvirt driver has no mediated-device type configured,
/// so `_update_provider_tree_for_vgpu` returns before touching a compute's tree, and its
/// PCI healing only rewrites providers carrying its own trait. The flip side is that stackbox
/// has to write them again whenever the tree could have changed: after a deploy, when a
/// compute joins, and after a restore. The writes are idempotent, and the inventory of an
/// existing provider is updated in place, so a re-run keeps whatever is allocated.
///
/// Empty for `mtty`, whose providers Nova builds from the mediated device it finds.
fn gpu_advertise_script(gpu: Option<&crate::model::GpuConfig>, hosts: &[String]) -> String {
    let Some(gpu) = gpu.filter(|gpu| gpu.mode == crate::model::GpuMode::Simulated) else {
        return String::new();
    };
    let class = gpu.resource_class();
    let traits = gpu.traits().join(",");
    let cards = gpu.device_totals();
    // `<host>:<provider>=<total>,<provider>=<total>`: one argument per compute, so the
    // Python side reads names and counts without a second format.
    let specs = hosts
        .iter()
        .map(|host| {
            let providers = cards
                .iter()
                .enumerate()
                .map(|(device, total)| {
                    format!(
                        "{}={total}",
                        crate::model::GpuConfig::provider_name(host, device as u32)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{host}:{providers}")
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"step "GPU(simulated) Placement 리소스 프로바이더 광고 ({class}×{count}, 가짜 장치 {devices}개)"
placement=$($os endpoint list --service placement --interface internal -f value -c URL -c url | awk 'NR == 1 {{print $1}}')
[ -n "$placement" ] || {{ echo "no placement endpoint" >&2; exit 1; }}
token=$($os token issue -f value -c id)
python3 - {class} "{traits}" "$placement" "$token" {specs} <<'PY'
import json
import sys
import time
import urllib.error
import urllib.request

resource_class, trait_list, placement, token = sys.argv[1:5]
traits = [trait for trait in trait_list.split(",") if trait]
# The child provider name and its inventory total, per compute host.
wanted = []
for spec in sys.argv[5:]:
    host, providers = spec.split(":", 1)
    wanted.append((host, [card.split("=") for card in providers.split(",")]))


def request(method, path, body=None, allow=()):
    """One Placement request: its parsed body and the answer's Location header."""
    payload = None if body is None else json.dumps(body).encode()
    headers = {{
        "X-Auth-Token": token,
        "OpenStack-API-Version": "placement 1.14",
    }}
    # A bodyless request (a trait PUT) carries no content type either.
    if payload is not None:
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(
        placement.rstrip("/") + path,
        data=payload,
        method=method,
        headers=headers,
    )
    try:
        answer = urllib.request.urlopen(request, timeout=60)
    except urllib.error.HTTPError as error:
        if error.code in allow:
            return None, None
        sys.exit(
            "%s %s: %s %s"
            % (method, path, error.code, error.read().decode().strip())
        )
    except urllib.error.URLError as error:
        # A refused connection or an unreachable endpoint: name the request that failed
        # instead of ending the step with a traceback.
        sys.exit("%s %s: %s" % (method, path, error.reason))
    except TimeoutError:
        sys.exit("%s %s: no answer within 60 seconds" % (method, path))
    body = answer.read()
    return (json.loads(body) if body else {{}}), answer.headers.get("Location")


def call(method, path, body=None, allow=()):
    """One Placement request's body; a refusal ends the step with its own message."""
    return request(method, path, body, allow)[0]


def named(name):
    """The uuid of the provider called `name`, if Placement has it."""
    for provider in call("GET", "/resource_providers?name=" + name)[
        "resource_providers"
    ]:
        return provider["uuid"]
    return None


# nova-compute reports a compute's resource provider when it first runs; the child providers
# hang off it, so wait for every compute of the stackbox.
providers = {{}}
for _ in range(60):
    providers = {{
        provider["name"]: provider["uuid"]
        for provider in call("GET", "/resource_providers")["resource_providers"]
    }}
    if all(host in providers for host, _ in wanted):
        break
    time.sleep(5)
# A resource provider accepts a trait only when the trait table holds it, and Nova
# registers just the traits its own providers carry. `PUT /traits/<name>` creates a
# missing one (201) and accepts an existing one (204), so this is safe on every run.
for trait in traits:
    call("PUT", "/traits/" + trait)

for host, cards in wanted:
    for name, total in cards:
        total = int(total)
        uuid = providers.get(name)
        if uuid is None:
            created, location = request(
                "POST",
                "/resource_providers",
                {{"name": name, "parent_provider_uuid": providers[host]}},
                allow=(409,),
            )
            if created is None:
                # A provider of that name exists already, which this run did not list.
                uuid = named(name)
                if uuid is None:
                    sys.exit("resource provider %s exists but cannot be read" % name)
            else:
                # From microversion 1.20 on Placement answers a create with the provider;
                # 1.14, which `parent_provider_uuid` needs here no more than 1.20 does,
                # answers 201 with an empty body and the new provider's URL in Location.
                uuid = created.get("uuid")
                if not uuid and location:
                    uuid = location.rstrip("/").rsplit("/", 1)[-1]
                if not uuid:
                    uuid = named(name)
                if not uuid:
                    sys.exit("%s: created, but Placement named no uuid" % name)
                print("%s: created as a child of %s" % (name, host), flush=True)
            providers[name] = uuid
        # Placement takes an inventory and a trait list only with the provider's generation,
        # which every write bumps.
        generation = call("GET", "/resource_providers/" + uuid)["generation"]
        call(
            "PUT",
            "/resource_providers/" + uuid + "/inventories",
            {{
                "inventories": {{
                    resource_class: {{
                        "total": total,
                        "reserved": 0,
                        "min_unit": 1,
                        "max_unit": total,
                        "step_size": 1,
                        "allocation_ratio": 1.0,
                    }}
                }},
                "resource_provider_generation": generation,
            }},
        )
        generation = call("GET", "/resource_providers/" + uuid)["generation"]
        call(
            "PUT",
            "/resource_providers/" + uuid + "/traits",
            {{"traits": traits, "resource_provider_generation": generation}},
        )
        print(
            "%s: %s=%d, traits %s"
            % (name, resource_class, total, ",".join(traits) or "none"),
            flush=True,
        )
print(
    "advertised %d GPU provider(s) on %d compute(s)"
    % (sum(len(cards) for _, cards in wanted), len(wanted))
)
PY
"#,
        count = gpu.count,
        devices = cards.len(),
    )
}

/// A GPU stackbox's own verification: Placement carries the mode's resource class on the child
/// provider of every fake GPU, then a flavor of the mode boots an instance and the
/// instance's allocation names that class on one of those providers. A flavor asking for one
/// unit more than any provider holds is refused with `NoValidHost`, which proves the
/// inventory is what the scheduler reads. `mtty` additionally proves libvirt attached the
/// mediated device to the instance's domain and, on a release whose Nova needed stackbox's
/// backport, that the running nova-compute carries it; the guest-side `lspci` check is left
/// to the end-to-end test, which reads the console log.
fn gpu_verify_script(gpu: &crate::model::GpuConfig, computes: usize, release: &str) -> String {
    let class = gpu.resource_class();
    let flavor = gpu.flavor();
    let count = gpu.count;
    let mode = gpu.mode_name();
    let mut trait_properties = String::new();
    for trait_name in gpu.traits() {
        trait_properties.push_str(&format!(" --property trait:{trait_name}=required"));
    }
    let properties = format!("--property resources:{class}=1{trait_properties}");
    let hosts: Vec<String> = (1..=computes)
        .map(|index| format!("stackbox-compute-{index}"))
        .collect();
    let joined = hosts.join(" ");
    let inventory = match gpu.mode {
        crate::model::GpuMode::Simulated => simulated_inventory_check(gpu, &hosts),
        crate::model::GpuMode::Mtty => mtty_inventory_check(gpu, &joined),
    };
    let overflow = match gpu.mode {
        // One unit more than any single provider holds: the unsuffixed group of a flavor
        // takes its virtual GPUs from one provider, so this cannot be scheduled anywhere.
        crate::model::GpuMode::Simulated => format!(
            r#"
step "GPU(simulated) 재고를 넘는 요청이 NoValidHost로 거절되는지 확인 ({class}×{over})"
over='stackbox.vgpu-full'
$os flavor show "$over" >/dev/null 2>&1 || $os flavor create --ram 512 --disk 1 --vcpus 1 "$over" >/dev/null
$os flavor set "$over" --property resources:{class}={over}{trait_properties} >/dev/null
full="stackbox-gpu-full-$(cut -c1-8 /proc/sys/kernel/random/uuid)"
# The API accepts the request and the scheduler refuses it, so the answer is the instance's
# fault, not this command's exit status.
$os server create --flavor "$over" --image cirros-stackbox --network "$net_id" "$full" --wait >/dev/null 2>&1 || true
full_id=$($os server list --name "$full" -f value -c ID | awk 'NR == 1 {{print $1}}')
[ -n "$full_id" ] || {{ echo "the oversubscribed instance was not created" >&2; exit 1; }}
[ "$($os server show "$full_id" -f value -c status)" = ERROR ] || {{ echo "a {class} request larger than the inventory was scheduled" >&2; exit 1; }}
$os server show "$full_id" -f value -c fault | grep "No valid host" >/dev/null || {{ echo "the oversubscribed instance failed for another reason" >&2; $os server show "$full_id" -f value -c fault >&2; exit 1; }}
$os server delete "$full_id" >/dev/null
echo "a {class} request of {over} was refused with NoValidHost"
"#,
            over = count + 1,
        ),
        crate::model::GpuMode::Mtty => String::new(),
    };
    let mtty_check = match gpu.mode {
        crate::model::GpuMode::Mtty => format!(
            r#"bound=''
for host in {joined}; do
  # grep reads the whole XML: with -q it would leave at its first match, and the ssh still
  # writing the other domains' XML would end 255, which `pipefail` fails the check with.
  if ssh -o BatchMode=yes -o ConnectTimeout=10 "$host" "docker exec nova_libvirt sh -c 'for d in \$(virsh list --name); do virsh dumpxml \"\$d\"; done'" 2>/dev/null | grep "type='mdev'" >/dev/null; then
    bound=$host
    break
  fi
done
[ -n "$bound" ] || {{ echo "no libvirt domain on {joined} has an mdev hostdev" >&2; exit 1; }}
echo "mtty mdev is bound to the instance's libvirt domain on $bound"
"#
        ),
        crate::model::GpuMode::Simulated => String::new(),
    };
    // Releases before 2026.2 get `mtty` support from the patched files stackbox mounts into
    // nova_compute; a container that started without them has different bytes here.
    let backport = match (gpu.mode, config::nova_mtty_mounts(release)) {
        (crate::model::GpuMode::Mtty, Some(mounts)) => {
            let copies: Vec<&str> = mounts.iter().map(|(host, _)| host.as_str()).collect();
            let targets: Vec<&str> = mounts
                .iter()
                .map(|(_, container)| container.as_str())
                .collect();
            format!(
                r#"
step "GPU(mtty) nova-compute의 Nova mtty 백포트 확인 ({release})"
for host in {joined}; do
  ssh -o BatchMode=yes -o ConnectTimeout=10 "$host" \
    "a=\$(docker exec nova_compute sha256sum {targets} | cut -d' ' -f1) && b=\$(sha256sum {copies} | cut -d' ' -f1) && [ -n \"\$a\" ] && [ \"\$a\" = \"\$b\" ]" \
    2>&1 || {{ echo "$host: nova-compute is not running with stackbox's Nova mtty backport files ({release}); see docker logs nova_compute" >&2; exit 1; }}
done
echo "mtty backport files are in place on {joined}"
"#,
                targets = targets.join(" "),
                copies = copies.join(" "),
            )
        }
        _ => String::new(),
    };
    format!(
        r#"{backport}{inventory}
step "GPU({mode}) flavor {flavor} 인스턴스와 할당 확인 ({class})"
$os flavor show {flavor} >/dev/null 2>&1 || $os flavor create --ram 512 --disk 1 --vcpus 1 {properties} {flavor} >/dev/null
# A flavor that already existed may carry other or missing properties; set ours again so
# the instance is scheduled on the class this script just checked.
$os flavor set {flavor} {properties} >/dev/null
name="stackbox-gpu-$(cut -c1-8 /proc/sys/kernel/random/uuid)"
net_id=''; subnet_id=''; router_id=''; server_id=''; full_id=''
extra_cleanup() {{
  if [ -n "$server_id" ]; then $os server delete "$server_id" >/dev/null 2>&1 || true; fi
  if [ -n "$full_id" ]; then $os server delete "$full_id" >/dev/null 2>&1 || true; fi
  if [ -n "$router_id" ]; then
    if [ -n "$subnet_id" ]; then $os router remove subnet "$router_id" "$subnet_id" >/dev/null 2>&1 || true; fi
    $os router delete "$router_id" >/dev/null 2>&1 || true
  fi
  if [ -n "$net_id" ]; then $os network delete "$net_id" >/dev/null 2>&1 || true; fi
}}
net_id=$($os network create "$name" -f value -c id)
subnet_id=$($os subnet create --network "$net_id" --subnet-range 10.197.0.0/24 "$name" -f value -c id)
router_id=$($os router create --external-gateway public "$name" -f value -c id)
$os router add subnet "$router_id" "$subnet_id" >/dev/null
# CirrOS does not apply Nova vendor data, so the verification instance gets the same
# installer a user would pass as user-data. It prints the first XML document to the
# console, which the check below reads.
user_data="$work/stackbox-gpu-user-data.sh"
cat > "$user_data" <<'STACKBOX_GPU_USER_DATA_EOF'
{installer}
STACKBOX_GPU_USER_DATA_EOF
server_id=$($os server create --flavor {flavor} --image cirros-stackbox --network "$net_id" --user-data "$user_data" "$name" -f value -c id)
status=''
for _ in $(seq 90); do
  status=$($os server show "$server_id" -f value -c status)
  case "$status" in
    ACTIVE) break ;;
    ERROR) echo "GPU instance entered ERROR" >&2; $os server show "$server_id" -f value -c fault >&2; exit 1 ;;
  esac
  sleep 10
done
[ "$status" = ACTIVE ] || {{ echo "GPU instance status: ${{status:-missing}}" >&2; exit 1; }}
python3 - {class} "$placement" "$token" "$server_id" "$work/gpu-providers" <<'PY'
import json
import os
import sys
import urllib.request

resource_class, placement, token, consumer, provider_list = sys.argv[1:6]


def get(path):
    request = urllib.request.Request(
        placement.rstrip("/") + path,
        headers={{
            "X-Auth-Token": token,
            "OpenStack-API-Version": "placement 1.14",
        }},
    )
    return json.load(urllib.request.urlopen(request, timeout=30))


# The providers this stackbox advertised. An mdev stackbox leaves the file empty, and then any
# provider holding the class is the one Nova built.
watched = set()
if os.path.exists(provider_list):
    with open(provider_list) as handle:
        watched = {{line.split()[1] for line in handle if line.strip()}}
for uuid, body in get("/allocations/" + consumer)["allocations"].items():
    if body["resources"].get(resource_class, 0) >= 1:
        if watched and uuid not in watched:
            sys.exit(
                "%s is allocated on %s, which this stackbox did not advertise"
                % (resource_class, uuid)
            )
        print(
            "%s allocated to the instance on %s"
            % (resource_class, "a child GPU provider" if watched else uuid)
        )
        break
else:
    sys.exit("the instance holds no %s allocation" % resource_class)
PY
{overflow}{mtty_check}
step "GPU({mode}) 게스트의 가짜 nvidia-smi 콘솔 출력 확인"
console_ok=''
for _ in $(seq 60); do
  console=$($os console log show "$server_id" 2>/dev/null || true)
  case "$console" in *"<nvidia_smi_log>"*) console_ok=1; break ;; esac
  sleep 2
done
[ -n "$console_ok" ] || {{ echo "the GPU instance's console has no nvidia-smi -q -x output; its user-data did not install the fake tool" >&2; exit 1; }}
echo "the instance's fake nvidia-smi printed its XML to the console"
extra_cleanup
server_id=''; full_id=''; router_id=''; net_id=''
echo "GPU OK: {mode} flavor {flavor} booted an instance with {class}"
"#,
        installer = crate::gpu::user_data_installer(),
    )
}

/// The PCI half of the verification: a `stackbox.pci` flavor per `[pci] alias` (the first is
/// `stackbox.pci`, the rest `stackbox.pci-2`, ...), one instance of `stackbox.pci` that must reach ACTIVE, and
/// its libvirt domain on `host` must carry a `type='pci'` hostdev. The instance is deleted; the
/// flavors stay for the user.
fn pci_verify_script(host: &str, devices: &[GuestPciDevice]) -> String {
    let mut flavors = String::new();
    for alias in config::pci_aliases(devices) {
        let flavor = format!("stackbox.{}", alias.name.trim_start_matches("stackbox-"));
        flavors.push_str(&format!(
            r#"step "PCI passthrough 플레이버 {flavor} (alias {name})"
$os flavor show {flavor} >/dev/null 2>&1 || $os flavor create --ram 512 --disk 1 --vcpus 1 {flavor} >/dev/null
$os flavor set {flavor} --property "pci_passthrough:alias={name}:1" >/dev/null
"#,
            name = alias.name,
        ));
    }
    format!(
        r#"{flavors}
step "PCI passthrough 검증 인스턴스 (stackbox.pci)와 libvirt hostdev 확인 ({host})"
pci_net=''; pci_subnet=''; pci_router=''; pci_server=''
pci_cleanup() {{
  if [ -n "$pci_server" ]; then $os server delete --wait "$pci_server" >/dev/null 2>&1 || true; fi
  if [ -n "$pci_router" ]; then
    if [ -n "$pci_subnet" ]; then $os router remove subnet "$pci_router" "$pci_subnet" >/dev/null 2>&1 || true; fi
    $os router delete "$pci_router" >/dev/null 2>&1 || true
  fi
  if [ -n "$pci_net" ]; then $os network delete "$pci_net" >/dev/null 2>&1 || true; fi
}}
trap 'pci_cleanup; cleanup' EXIT
pci_name="stackbox-pci-verify-$(cut -c1-8 /proc/sys/kernel/random/uuid)"
pci_net=$($os network create "$pci_name" -f value -c id)
pci_subnet=$($os subnet create --network "$pci_net" --subnet-range 10.196.0.0/24 "$pci_name" -f value -c id)
pci_router=$($os router create --external-gateway public "$pci_name" -f value -c id)
$os router add subnet "$pci_router" "$pci_subnet" >/dev/null
pci_server=$($os server create --flavor stackbox.pci --image cirros-stackbox --network "$pci_net" "$pci_name" -f value -c id)
status=''
for _ in $(seq 90); do
  status=$($os server show "$pci_server" -f value -c status)
  case "$status" in
    ACTIVE) break ;;
    ERROR) echo "PCI instance entered ERROR" >&2; $os server show "$pci_server" -f value -c fault >&2; exit 1 ;;
  esac
  sleep 10
done
[ "$status" = ACTIVE ] || {{ echo "PCI instance status: ${{status:-missing}}" >&2; exit 1; }}
inst=$($os server show "$pci_server" -f value -c OS-EXT-SRV-ATTR:instance_name)
pci_host={host}
# The whole XML is read into a variable before it is matched: no reader leaves a pipe early.
xml=$(ssh -o BatchMode=yes -o ConnectTimeout=10 "$pci_host" "docker exec nova_libvirt virsh dumpxml $inst" 2>/dev/null) || {{ echo "cannot read libvirt domain $inst on $pci_host" >&2; exit 1; }}
case "$xml" in
  *"<hostdev mode='subsystem' type='pci'"*) ;;
  *) echo "libvirt domain $inst on $pci_host has no PCI hostdev" >&2; exit 1 ;;
esac
echo "PCI hostdev is bound to the instance's libvirt domain on $pci_host"
$os server delete --wait "$pci_server" >/dev/null
pci_server=''
echo "stackbox.pci flavor kept for use"
"#
    )
}

/// The child-provider half of [`gpu_verify_script`] for a `simulated` stackbox: every fake GPU of
/// every compute is a child of that compute's own provider, carrying exactly the inventory
/// and the traits the plan asked for, and no compute's root provider carries the class (Nova
/// replaces that inventory every sync, so a stackbox that wrote there would lose it).
fn simulated_inventory_check(gpu: &crate::model::GpuConfig, hosts: &[String]) -> String {
    let class = gpu.resource_class();
    let traits = gpu.traits().join(",");
    let cards = gpu.device_totals();
    let specs = hosts
        .iter()
        .map(|host| {
            let providers = cards
                .iter()
                .enumerate()
                .map(|(device, total)| {
                    format!(
                        "{}={total}",
                        crate::model::GpuConfig::provider_name(host, device as u32)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{host}:{providers}")
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"step "GPU(simulated) 자식 리소스 프로바이더 확인 ({class}×{count}, 가짜 장치 {devices}개)"
placement=$($os endpoint list --service placement --interface internal -f value -c URL -c url | awk 'NR == 1 {{print $1}}')
[ -n "$placement" ] || {{ echo "no placement endpoint" >&2; exit 1; }}
token=$($os token issue -f value -c id)
python3 - {class} "{traits}" "$work/gpu-providers" "$placement" "$token" {specs} <<'PY'
import json
import sys
import urllib.request

resource_class, trait_list, out, placement, token = sys.argv[1:6]
traits = {{trait for trait in trait_list.split(",") if trait}}
# The child provider name and its inventory total, per compute host.
wanted = []
for spec in sys.argv[6:]:
    host, providers = spec.split(":", 1)
    wanted.append((host, [card.split("=") for card in providers.split(",")]))


def get(path):
    request = urllib.request.Request(
        placement.rstrip("/") + path,
        headers={{
            "X-Auth-Token": token,
            "OpenStack-API-Version": "placement 1.14",
        }},
    )
    return json.load(urllib.request.urlopen(request, timeout=30))


providers = {{
    provider["name"]: provider
    for provider in get("/resource_providers")["resource_providers"]
}}
lines = []
for host, cards in wanted:
    root = providers.get(host)
    if root is None:
        sys.exit("no resource provider for %s: nova-compute did not report" % host)
    if resource_class in get(
        "/resource_providers/" + root["uuid"] + "/inventories"
    )["inventories"]:
        sys.exit(
            "%s carries %s on its root provider, which Nova rewrites"
            % (host, resource_class)
        )
    for name, total in cards:
        total = int(total)
        provider = providers.get(name)
        if provider is None:
            sys.exit("no resource provider %s" % name)
        if provider.get("parent_provider_uuid") != root["uuid"]:
            sys.exit("%s is not a child of %s" % (name, host))
        inventory = get(
            "/resource_providers/" + provider["uuid"] + "/inventories"
        )["inventories"]
        held = inventory.get(resource_class, {{}}).get("total")
        if held != total:
            sys.exit("%s holds %s=%s, expected %d" % (name, resource_class, held, total))
        # Placement answers a provider's trait list as strings on every microversion.
        present = set(
            get("/resource_providers/" + provider["uuid"] + "/traits")["traits"]
        )
        missing = sorted(traits - present)
        if missing:
            sys.exit("%s lacks trait(s) %s" % (name, ", ".join(missing)))
        lines.append((name, provider["uuid"]))
with open(out, "w") as handle:
    handle.write("\n".join("%s %s" % line for line in lines) + "\n")
print(
    "%s on %d child provider(s) of %d compute(s): %s"
    % (
        resource_class,
        len(lines),
        len(wanted),
        ", ".join(
            "%s=%d" % (card.rsplit("_pci_", 1)[-1], int(total))
            for _, cards in wanted
            for card, total in cards
        ),
    )
)
PY
"#,
        count = gpu.count,
        devices = cards.len(),
    )
}

/// The provider half of [`gpu_verify_script`] for `mtty`: Nova built these from the mediated
/// device it found, so the check only asks that every compute reports the class somewhere in
/// its tree, at least the requested amount.
fn mtty_inventory_check(gpu: &crate::model::GpuConfig, hosts: &str) -> String {
    let class = gpu.resource_class();
    let count = gpu.count;
    let expected = hosts.replace(' ', ",");
    format!(
        r#"step "GPU(mtty) Placement 인벤토리 확인 ({class}×{count})"
placement=$($os endpoint list --service placement --interface internal -f value -c URL -c url | awk 'NR == 1 {{print $1}}')
[ -n "$placement" ] || {{ echo "no placement endpoint" >&2; exit 1; }}
token=$($os token issue -f value -c id)
python3 - {class} {count} {expected} "$placement" "$token" <<'PY'
import json
import sys
import urllib.request

resource_class, count, expected, placement, token = sys.argv[1:6]
count = int(count)
expected = set(expected.split(","))

def get(path):
    request = urllib.request.Request(
        placement.rstrip("/") + path,
        headers={{
            "X-Auth-Token": token,
            "OpenStack-API-Version": "placement 1.4",
        }},
    )
    return json.load(urllib.request.urlopen(request, timeout=30))

found = {{}}
for provider in get("/resource_providers")["resource_providers"]:
    # A compute's inventory is on its own provider; an mdev type Nova discovers gets a
    # child provider named after it (<host>_<type>), which reports the same compute.
    host = provider["name"].split("_", 1)[0]
    if host not in expected:
        continue
    inventories = get(
        "/resource_providers/" + provider["uuid"] + "/inventories"
    )["inventories"]
    if resource_class in inventories and inventories[resource_class]["total"] >= count:
        found[host] = found.get(host, 0) + inventories[resource_class]["total"]
if set(found) != expected:
    sys.exit(
        "GPU inventory %s is missing on %s"
        % (resource_class, ", ".join(sorted(expected - set(found))))
    )
# The class total a compute offers, not the units this check asked for: the kernel's mdev
# type carries 24 whether the stackbox requested 1 or 24.
totals = sorted(set(found.values()))
if len(totals) == 1:
    counted = "%d each" % totals[0]
else:
    counted = ", ".join("%s=%d" % (host, found[host]) for host in sorted(found))
print(
    "%s inventory on %d compute resource(s): %s"
    % (resource_class, len(found), counted)
)
PY
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[test]
    fn images_published_for_follows_the_arch_each_release_publishes() {
        for release in ["2026.2", "2026.1", "2025.2", "2025.1", "2024.2", "2024.1"] {
            let info = releases::info(release);
            assert!(images_published_for("x86_64", &info), "{release} x86_64");
            assert!(!images_published_for("arm64", &info), "{release} arm64");
            assert!(!images_published_for("riscv64", &info), "{release} riscv64");
            assert!(!images_published_for("", &info), "{release} empty");
        }
        for release in ["2026.2", "2026.1", "2025.1"] {
            assert!(
                images_published_for("aarch64", &releases::info(release)),
                "{release} aarch64"
            );
        }
        // 2025.2's Rocky 10 aarch64 build fails from kolla's sources, so its images are amd64 only.
        assert!(!images_published_for("aarch64", &releases::info("2025.2")));
        assert!(!images_published_for("aarch64", &releases::info("2024.2")));
        assert!(!images_published_for("aarch64", &releases::info("2024.1")));
    }

    /// Runs the removal the stackbox sends for `nova.conf` through a real shell, so the test sees
    /// what the guest's shell does with a managed file, an operator's file and no file.
    #[test]
    fn nova_global_is_removed_only_when_the_stackbox_wrote_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nova.conf");
        let path = path.to_str().unwrap();
        let remove = |path: &str| {
            let status = Command::new("sh")
                .args(["-c", &remove_managed_nova_global(path)])
                .status()
                .unwrap();
            assert!(status.success(), "{status}");
        };

        // A file the stackbox wrote carries the marker on its first line and goes.
        let device = GuestPciDevice {
            bdf: "0000:00:05.0".into(),
            vendor_id: "10de".into(),
            product_id: "1eb8".into(),
            iommu_group: 3,
        };
        let managed = config::nova_global_conf(&[device]).unwrap();
        std::fs::write(path, managed).unwrap();
        remove(path);
        assert!(!std::path::Path::new(path).exists());

        // An operator's own nova.conf stays, even though it is not empty.
        std::fs::write(path, "[DEFAULT]\ndebug = true\n").unwrap();
        remove(path);
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "[DEFAULT]\ndebug = true\n"
        );

        // A marker that is not on the first line does not count.
        std::fs::write(path, format!("[DEFAULT]\n{}\n", config::NOVA_GLOBAL_MARKER)).unwrap();
        remove(path);
        assert!(std::path::Path::new(path).exists());

        // With no file at all the removal succeeds.
        std::fs::remove_file(path).unwrap();
        remove(path);
    }

    /// The guest's VFIO nodes belong to the nova_libvirt group, 42427, and no one else may
    /// open them: the prepare script and the one Kolla runs at deploy both set `0660`.
    #[test]
    fn the_guest_vfio_nodes_are_group_42427_and_0660() {
        let script = include_str!("../../guest/stackbox/stackbox-vfio-prepare");
        assert!(script.contains("chown root:42427"), "{script}");
        assert!(script.contains("chmod 0660"), "{script}");
        assert!(!script.contains("0666"), "{script}");
        let deploy = include_str!("mod.rs");
        let vfio_line = deploy
            .lines()
            .find(|line| line.starts_with("if [ -e /dev/vfio/vfio ]; then chown root:42427"))
            .unwrap_or_default();
        assert!(vfio_line.contains("chmod 0660"), "{vfio_line}");
    }

    /// Walks `<<'PY'` blocks and rejects an opener inside another block.
    fn assert_heredocs_do_not_nest(script: &str) {
        let mut open = false;
        for line in script.lines() {
            let trimmed = line.trim();
            if trimmed == "PY" {
                open = false;
                continue;
            }
            if trimmed.ends_with("<<'PY'") {
                assert!(!open, "nested python heredoc in generated script");
                open = true;
            }
        }
        assert!(!open, "unterminated python heredoc in generated script");
    }

    #[test]
    fn skyline_verification_logs_in_on_the_internal_apiserver() {
        let with = verify_script(
            &["keystone", "glance", "skyline"].map(String::from),
            1,
            OPENSTACK_RELEASE,
            None,
        );
        assert!(with.contains("Skyline 로그인 확인"));
        assert!(with.contains("kolla_internal_vip_address"));
        assert!(with.contains("\"$skyline/api/v1/login\""));
        assert!(with.contains(r#"\"domain\": \"Default\", \"username\": \"admin\""#));
        assert!(with.contains("skyline login returned $status"));
        let without = verify_script(
            &["keystone", "glance"].map(String::from),
            1,
            OPENSTACK_RELEASE,
            None,
        );
        assert!(!without.contains("api/v1/login"));
    }

    #[test]
    fn a_failure_after_configure_also_names_configure_as_the_resume_point() {
        let after = resume_hint("demo", Phase::Deploy);
        assert_eq!(
            after,
            "phase 'deploy' failed; resume with `stackbox deploy demo --from deploy`; if you changed the configuration, resume with `stackbox deploy demo --from configure`"
        );
        assert!(resume_hint("demo", Phase::Verify).contains("--from configure"));
        assert!(resume_hint("demo", Phase::Prechecks).contains("--from configure"));
        let before = resume_hint("demo", Phase::Configure);
        assert_eq!(
            before,
            "phase 'configure' failed; resume with `stackbox deploy demo --from configure`"
        );
        assert!(!resume_hint("demo", Phase::Install).contains("configuration"));
    }

    #[test]
    fn only_a_release_whose_tooling_predates_the_guest_python_installs_its_own() {
        assert_eq!(
            python_command(&releases::info(OPENSTACK_RELEASE)),
            "python3"
        );
        assert_eq!(python_command(&releases::info("2021.1")), "python3.9");
        // Both 2024.x series run their tooling on the guest's 3.12.
        for release in ["2024.1", "2024.2"] {
            assert_eq!(python_command(&releases::info(release)), "python3");
            assert!(!python_setup(&releases::info(release)).contains("uv"));
        }

        // An end-of-life series needs a minor the guest distribution does not ship, so the
        // installer places it itself instead of asking for a golden-image rebuild.
        let legacy = python_setup(&releases::info("2020.1"));
        assert!(legacy.contains("python=python3.8"), "{legacy}");
        assert!(legacy.contains("/opt/python3.8/bin/python3.8"), "{legacy}");
        assert!(legacy.contains("/usr/local/bin/python3.8"), "{legacy}");
        // The prefix used to be doubled, pointing at /opt/pythonpython3.8/bin/pythonpython3.8.
        assert!(!legacy.contains("pythonpython"), "{legacy}");
        assert!(
            legacy.contains("UV_PYTHON_INSTALL_DIR=/opt/stackbox-python"),
            "{legacy}"
        );
        assert!(
            legacy.contains("/opt/stackbox-uv/bin/uv python install 3.8"),
            "{legacy}"
        );
        assert!(
            legacy.contains(r#"python="$(/opt/stackbox-uv/bin/uv python find 3.8"#),
            "{legacy}"
        );
        // uv comes from a venv of its own: Rocky refuses pip into the system interpreter.
        assert!(
            legacy.contains("python3 -m venv /opt/stackbox-uv"),
            "{legacy}"
        );
        assert!(
            legacy.contains(r#"step "Python 3.8 준비 (uv)""#),
            "{legacy}"
        );
        // Without uv the installer stops instead of building a venv from the wrong Python.
        assert!(legacy.contains("which uv could not install"), "{legacy}");

        // A current release runs on the guest's own python3 and carries none of that.
        let current = python_setup(&releases::info(OPENSTACK_RELEASE));
        assert!(current.contains("python=python3"), "{current}");
        assert!(!current.contains("uv"), "{current}");
        assert!(!current.contains("stackbox-python"), "{current}");
        assert!(!install_script(&releases::info(OPENSTACK_RELEASE)).contains("uv"));
        // The failure names both hosts uv needs: PyPI for uv itself, GitHub for the Python.
        assert!(legacy.contains("pypi.org"), "{legacy}");
        assert!(legacy.contains("github.com"), "{legacy}");
    }

    #[test]
    fn a_legacy_series_installs_the_ansible_its_quickstart_asks_for() {
        // The tags do not declare Ansible; each series' quickstart installs this range into
        // the same venv (doc/source/user/quickstart.rst at the tag).
        for (release, spec) in [
            ("2020.1", "ansible>=2.8,<2.10"),
            ("2020.2", "ansible>=2.9,<2.10"),
            ("2021.1", "ansible>=2.9,<3.0"),
            ("2021.2", "ansible>=2.10,<5.0"),
            ("2022.1", "ansible>=4,<6"),
            ("2022.2", "ansible>=4,<6"),
        ] {
            let info = releases::info(release);
            assert_eq!(info.ansible, spec);
            let script = install_script(&info);
            assert!(
                script.contains(&format!(r#"/opt/kolla/bin/pip install -q "{spec}""#)),
                "{release}: {script}"
            );
            // The range is part of the stamp, so a venv built before it gets rebuilt.
            assert!(
                script.contains(&format!(
                    r#"stamp="{release} {} {spec} jinja2<3.1 markupsafe<3 $python""#,
                    info.reference
                )),
                "{release}: {script}"
            );
            // Ansible 2.9/2.10 call Jinja2's removed `environmentfilter`; the 3.0 line keeps it.
            assert!(
                script.contains(r#"pip install -q "ansible>=2" "Jinja2<3.1" "MarkupSafe<3""#)
                    || script.contains(r#""Jinja2<3.1" "MarkupSafe<3""#),
                "{release}: {script}"
            );
        }
        // A maintained branch declares Ansible itself and keeps the stamp it had.
        let current = install_script(&releases::info(OPENSTACK_RELEASE));
        assert!(!current.contains(r#"pip install -q "ansible"#), "{current}");
        assert!(
            current.contains(&format!(
                r#"stamp="{release} stable/{release} $python""#,
                release = OPENSTACK_RELEASE
            )),
            "{current}"
        );
    }

    #[test]
    fn install_deps_runs_only_where_the_launcher_has_it() {
        // yoga (14.11.0) added the subcommand for its Galaxy collections.
        for release in ["2022.2", "2022.1"] {
            let script = install_script(&releases::info(release));
            assert!(
                script.contains("\nkolla-ansible install-deps\n"),
                "{release}: {script}"
            );
        }
        // Older tags ship their roles in-tree and answer the subcommand with usage (exit 3).
        for release in ["2021.2", "2021.1", "2020.2", "2020.1"] {
            let script = install_script(&releases::info(release));
            assert!(
                !script.contains("\nkolla-ansible install-deps\n"),
                "{release}: {script}"
            );
            assert!(script.contains("install-deps 생략"), "{release}: {script}");
        }
        assert!(
            install_script(&releases::info(OPENSTACK_RELEASE))
                .contains("\nkolla-ansible install-deps\n")
        );
    }

    #[test]
    fn an_end_of_life_galaxy_collection_is_installed_from_its_surviving_tag() {
        for (release, series, tag) in [("2022.2", "zed", "zed-eol"), ("2022.1", "yoga", "yoga-eol")]
        {
            let script = install_script(&releases::info(release));
            // The tag's requirements.yml names a branch the collection deleted at end of life.
            assert!(
                script.contains(&format!(r#"marker = "version: stable/{series}""#)),
                "{release}: {script}"
            );
            assert!(
                script.contains(&format!(r#"if "version: {tag}" not in text:"#)),
                "{release}: {script}"
            );
            // It runs before the Galaxy step that reads the file.
            let rewrite = script.find("ansible-collection-kolla 참조 교체").unwrap();
            let deps = script.find("\nkolla-ansible install-deps\n").unwrap();
            assert!(rewrite < deps, "{release}: {script}");
        }
        // The tags before yoga have no requirements.yml; a maintained branch's names a branch
        // the collection still publishes. Neither rewrites anything.
        for release in ["2021.2", "2021.1", "2020.2", "2020.1", "2024.1", "2026.2"] {
            let script = install_script(&releases::info(release));
            assert!(!script.contains("requirements.yml"), "{release}: {script}");
        }
    }

    #[test]
    fn the_galaxy_ref_rewrite_is_idempotent_and_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("requirements.yml");
        // `requirements.yml` at tag 15.6.0, byte for byte.
        let requirements = "---\ncollections:\n  - name: https://opendev.org/openstack/ansible-collection-kolla\n    type: git\n    version: stable/zed\n";
        std::fs::write(&file, requirements).unwrap();
        let program = python_program(&galaxy_ref_step(&releases::info("2022.2"))).replace(
            &format!("{VENV}/share/kolla-ansible"),
            &dir.path().display().to_string(),
        );
        let Some(result) = run_python(&program) else {
            return;
        };
        result.unwrap_or_else(|error| panic!("{error}"));
        let rewritten = std::fs::read_to_string(&file).unwrap();
        assert_eq!(rewritten, requirements.replace("stable/zed", "zed-eol"));
        // The venv keeps the rewritten file, so a retry must not trip over it.
        if let Some(result) = run_python(&program) {
            result.unwrap_or_else(|error| panic!("{error}"));
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), rewritten);
        // A release whose file is not the reviewed one fails the phase instead of letting
        // Galaxy clone a branch that no longer exists.
        std::fs::write(&file, requirements.replace("stable/zed", "stable/wallaby")).unwrap();
        let error = run_python(&program).unwrap().unwrap_err();
        assert!(
            error.contains("unexpected kolla-ansible requirements.yml"),
            "{error}"
        );
    }

    #[test]
    fn the_interpreter_is_resolved_before_the_venv_joins_path() {
        // On a retry {VENV}/bin still holds the previous venv's python3.8. Resolving after the
        // export stamps a bare name that never matches, and the script then deletes the venv
        // and the only interpreter with it (exit 127).
        let script = install_script(&releases::info("2020.1"));
        let resolve = script.find("python=python3.8").unwrap();
        let stamp = script.find(r#"stamp="2020.1 10.4.0"#).unwrap();
        let export = script.find("export PATH=/opt/kolla/bin:$PATH").unwrap();
        assert!(resolve < stamp && stamp < export, "{script}");
        assert_eq!(
            script.matches("export PATH=/opt/kolla/bin").count(),
            1,
            "{script}"
        );
        // Whatever PATH the retry starts from, the stamp keeps the absolute interpreter.
        assert!(
            script.contains("python=$(command -v \"$python\")"),
            "{script}"
        );
    }

    /// The Python program a generated script runs to patch the tree.
    fn python_program(script: &str) -> &str {
        script
            .split_once(" - <<'PY'\n")
            .unwrap()
            .1
            .split_once("\nPY")
            .unwrap()
            .0
    }

    /// Runs a generated patch program, returning its failure output when it fails. A host
    /// without python3 skips the test: the guest always has it.
    fn run_python(program: &str) -> Option<Result<(), String>> {
        let mut child = match Command::new("python3")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                eprintln!("python3 unavailable, skipping the patch test: {error}");
                return None;
            }
        };
        child
            .stdin
            .take()
            .unwrap()
            .write_all(program.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        Some(
            output
                .status
                .success()
                .then_some(())
                .ok_or_else(|| String::from_utf8_lossy(&output.stderr).into_owned()),
        )
    }

    /// Copies a tag's task files into a fresh tree at the paths the installer patches.
    fn task_tree(tag: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (fixture, path) in [
            (
                "openvswitch-post-config.yml",
                "roles/openvswitch/tasks/post-config.yml",
            ),
            (
                "openvswitch-config-host.yml",
                "roles/openvswitch/tasks/config-host.yml",
            ),
            (
                "nova-cell-precheck.yml",
                "roles/nova-cell/tasks/precheck.yml",
            ),
            (
                "nova-cell-config-host.yml",
                "roles/nova-cell/tasks/config-host.yml",
            ),
        ] {
            let target = dir.path().join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::copy(format!("tests/fixtures/kolla/{tag}/{fixture}"), &target).unwrap();
        }
        // The tags before yoga carry this task; later ones do not.
        let baremetal = format!("tests/fixtures/kolla/{tag}/baremetal-install.yml");
        if std::path::Path::new(&baremetal).exists() {
            let target = dir.path().join("roles/baremetal/tasks/install.yml");
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::copy(&baremetal, &target).unwrap();
        }
        // Every end-of-life tag ships the toolbox module; a maintained branch's fixture set
        // needs none.
        let toolbox = format!("tests/fixtures/kolla/{tag}/kolla_toolbox.py");
        if std::path::Path::new(&toolbox).exists() {
            let target = dir.path().join("library/kolla_toolbox.py");
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::copy(&toolbox, &target).unwrap();
        }
        dir
    }

    #[test]
    fn maintained_releases_keep_the_install_script_they_had() {
        // The end-of-life work must not change the install for a maintained release: these
        // fixtures pin what the stackbox generates for one now, so a change there shows up here.
        // Compared with the script this branch deployed before the work, the 2026.2 fixture
        // differs only in the interpreter handling the older series needed: the old
        // template's `/opt/pythonpython3` typo is fixed (the variable already held `python3`),
        // `python` is resolved to an absolute path before the stamp, and `PATH` is exported
        // after it so a retry keeps the venv.
        for (release, fixture) in [
            (
                "2026.2",
                include_str!("../../tests/fixtures/kolla/install-2026.2.sh"),
            ),
            (
                "2024.1",
                include_str!("../../tests/fixtures/kolla/install-2024.1.sh"),
            ),
        ] {
            assert_eq!(install_script(&releases::info(release)), fixture);
        }
        // The kolla-ansible invocations of a maintained branch keep the environment they had;
        // only a legacy tag's bash launcher needs VIRTUAL_ENV.
        let current = releases::info(OPENSTACK_RELEASE);
        assert!(!deploy_script(&current).contains("VIRTUAL_ENV"));
        assert!(!kolla_ansible("prechecks", &current).contains("VIRTUAL_ENV"));
        let legacy = releases::info("2020.1");
        assert!(deploy_script(&legacy).contains("VIRTUAL_ENV=/opt/kolla"));
        assert!(kolla_ansible("prechecks", &legacy).starts_with("VIRTUAL_ENV=/opt/kolla PATH="));
        assert!(install_script(&legacy).contains("export VIRTUAL_ENV=/opt/kolla"));
    }

    #[test]
    fn the_2024_series_install_from_the_refs_that_still_exist() {
        // 2024.1 kept an unmaintained branch, and its requirements.yml already names it.
        let first = releases::info("2024.1");
        assert_eq!(first.reference, "unmaintained/2024.1");
        let script = install_script(&first);
        assert!(
            script.contains("kolla-ansible@unmaintained/2024.1"),
            "{script}"
        );
        assert!(script.contains("constraints/upper/2024.1"), "{script}");
        assert!(!script.contains("stable/2024.1"), "{script}");
        // Its bash launcher needs VIRTUAL_ENV, and its requirements leave Ansible out: the
        // installer adds ansible-core 2.16, which supports the guest's Python 3.12.
        assert!(script.contains("python=python3\n"), "{script}");
        assert!(script.contains("export VIRTUAL_ENV=/opt/kolla"), "{script}");
        assert!(
            script.contains(r#"/opt/kolla/bin/pip install -q "ansible-core>=2.16,<2.17""#),
            "{script}"
        );
        assert!(!script.contains("Jinja2<3.1"), "{script}");

        // 2024.2's branch is gone: install its last tag, and rewrite the collection branch its
        // requirements.yml names to the last tagged commit of that branch.
        let last = releases::info("2024.2");
        assert_eq!(last.reference, "2024.2-eol");
        let script = install_script(&last);
        assert!(script.contains("kolla-ansible@2024.2-eol"), "{script}");
        assert!(script.contains("constraints/upper/2024.2"), "{script}");
        assert!(
            script.contains("stable/2024.2 → 6.3.0"),
            "the Galaxy ref is not rewritten: {script}"
        );
        assert!(script.contains("version: stable/2024.2"), "{script}");
        // It is a maintained tree, so none of the end-of-life tooling applies.
        assert!(!script.contains("Jinja2<3.1"), "{script}");
        assert!(!script.contains("libxcrypt-compat"), "{script}");
    }

    #[test]
    fn the_maintained_branch_patches_still_apply() {
        let info = releases::info(OPENSTACK_RELEASE);
        let dir = task_tree("current");
        let program = python_program(&install_script(&info)).replace(
            &format!("{VENV}/share/kolla-ansible/ansible"),
            &dir.path().display().to_string(),
        );
        if let Some(result) = run_python(&program) {
            result.unwrap_or_else(|error| panic!("{error}"));
        }
        let read = |path: &str| std::fs::read_to_string(dir.path().join(path)).unwrap();
        let post = read("roles/openvswitch/tasks/post-config.yml");
        assert!(post.contains("datapath_type={{ ovs_datapath }}"), "{post}");
        assert!(post.contains("userspace-tso-enable"), "{post}");
        assert!(
            read("roles/openvswitch/tasks/config-host.yml")
                .contains("when: ovs_datapath == 'system'")
        );
        assert!(read("roles/nova-cell/tasks/precheck.yml").contains("stackbox_no_computes"));
        assert!(read("roles/nova-cell/tasks/config-host.yml").contains("    - false\n"));
    }

    #[test]
    fn an_end_of_life_tag_is_patched_against_its_own_task_files() {
        for entry in releases::LEGACY {
            let info = releases::info(entry.release);
            let dir = task_tree(entry.kolla);
            let program = kernel_patches(&info, &dir.path().display().to_string());
            let Some(result) = run_python(python_program(&program)) else {
                return;
            };
            result.unwrap_or_else(|error| panic!("{}: {error}", entry.release));
            let read = |path: &str| std::fs::read_to_string(dir.path().join(path)).unwrap();
            let first = read("roles/openvswitch/tasks/post-config.yml");

            // The bridge is created with the stackbox's datapath type before Kolla's own task adds
            // the port; Kolla's helper would have created a kernel (system) datapath.
            assert!(
                first.contains("datapath_type={{ ovs_datapath }}"),
                "{}",
                entry.release
            );
            assert!(
                first.find("--may-exist add-br").unwrap()
                    < first
                        .find("- name: Ensuring OVS bridge is properly setup")
                        .unwrap()
            );
            assert_eq!(
                first
                    .matches("- name: Ensuring OVS bridge is properly setup")
                    .count(),
                1,
                "{}",
                entry.release
            );

            // One userspace-TSO setting: the loop item on yoga/zed, a task of its own before.
            assert_eq!(
                first.matches("userspace-tso-enable").count(),
                1,
                "{}",
                entry.release
            );
            if matches!(entry.kolla, "14.11.0" | "15.6.0") {
                assert!(
                    first.contains("    - { col: \"other_config\", name: \"userspace-tso-enable\""),
                    "{}",
                    entry.release
                );
            } else {
                assert!(
                    first.contains("other_config:userspace-tso-enable=true"),
                    "{}",
                    entry.release
                );
            }

            let precheck = read("roles/nova-cell/tasks/precheck.yml");
            assert!(
                precheck.contains("- not (stackbox_no_computes | default(false))"),
                "{}",
                entry.release
            );
            let hosts = read("roles/nova-cell/tasks/config-host.yml");
            assert!(hosts.contains("- name: Load and persist br_netfilter module\n  include_role:\n    name: module-load\n  vars:\n    modules:\n      - { name: br_netfilter }\n  when:\n    - false\n"), "{}", entry.release);
            assert!(read("roles/openvswitch/tasks/config-host.yml")
                .contains("- name: Load and persist openvswitch module\n  when: ovs_datapath == 'system'\n"), "{}", entry.release);
            // dnf5 dropped delta RPMs; the tags that still install drpm lose the task.
            if matches!(entry.kolla, "10.4.0" | "11.4.0" | "12.8.0" | "13.9.0") {
                let baremetal = read("roles/baremetal/tasks/install.yml");
                assert!(!baremetal.contains("name: drpm"), "{}", entry.release);
                assert!(
                    baremetal.contains("# stackbox: dnf5 dropped delta RPMs"),
                    "{}",
                    entry.release
                );
            }

            // The toolbox's Ansible prints warnings before the JSON document it returns, and
            // the module parses the whole output. The preamble is dropped, not an error.
            let toolbox = read("library/kolla_toolbox.py");
            assert!(
                toolbox.contains("start = json_output.find(marker)"),
                "{}",
                entry.release
            );
            assert_eq!(
                toolbox
                    .matches("json_output = client.exec_start(job)")
                    .count(),
                1,
                "{}",
                entry.release
            );

            // A retry after a failed phase finds the files patched and changes nothing.
            let Some(result) = run_python(python_program(&program)) else {
                return;
            };
            result.unwrap_or_else(|error| panic!("{} retry: {error}", entry.release));
            assert_eq!(
                read("roles/openvswitch/tasks/post-config.yml"),
                first,
                "{}",
                entry.release
            );
        }
    }

    #[test]
    fn generated_scripts_parse_with_optional_service_verification_local() {
        let components = [
            "keystone",
            "glance",
            "placement",
            "horizon",
            "skyline",
            "neutron",
            "nova",
        ]
        .map(String::from);
        let exposure = |port_base| Exposure {
            host: "localhost".into(),
            bind: [127, 0, 0, 1].into(),
            port_base,
        };
        let (from, to) = (exposure(15000), exposure(15010));
        let info = releases::info(OPENSTACK_RELEASE);
        let scripts = [
            install_script(&info),
            post_deploy_script(&components, false, &info),
            post_deploy_script(&components, true, &info),
            verify_script(&components, 1, OPENSTACK_RELEASE, None),
            verify_script(&components, 2, OPENSTACK_RELEASE, None),
            verify_script(&components, 0, OPENSTACK_RELEASE, None),
            verify_script(
                &components,
                1,
                OPENSTACK_RELEASE,
                Some(&crate::model::GpuConfig {
                    mode: crate::model::GpuMode::Simulated,
                    count: 4,
                    ..Default::default()
                }),
            ),
            verify_script(
                &components,
                2,
                OPENSTACK_RELEASE,
                Some(&crate::model::GpuConfig {
                    mode: crate::model::GpuMode::Mtty,
                    count: 24,
                    ..Default::default()
                }),
            ),
            settle_script(None, &components),
            settle_script(Some((&from, &to)), &components),
            forget_computes_script(&components),
            forget_computes_script(&components[..2]),
            OVS_EXTEND_START_SCRIPT.to_owned(),
            include_str!("../../guest/stackbox/stackbox-external").to_owned(),
            include_str!("../../guest/stackbox/stackbox-identity").to_owned(),
            // Deploying a stackbox and adding a compute read the guest's address the same way.
            identity_command(),
            // The deploy waits for OVS and the compute prepares its links: both poll commands
            // that keep writing after the answer is known.
            deploy_script(&info),
            RESTART_OVS_AGENT.to_owned(),
            verify_compute_script("stackbox-compute-1", true),
            // A compute booted from a template recreates the containers it inherited.
            wipe_template_script("stackbox-compute-2"),
            // Every end-of-life series installs from its last tag on its own Python, with the
            // libcrypt and Jinja2 fixes its interpreter and Ansible need.
            install_script(&releases::info("2022.2")),
            install_script(&releases::info("2022.1")),
            install_script(&releases::info("2021.2")),
            install_script(&releases::info("2021.1")),
            install_script(&releases::info("2020.2")),
            install_script(&releases::info("2020.1")),
        ];
        for script in scripts {
            // AGENTS.md: under `set -o pipefail` a reader that stops at its first match
            // (`grep -q`, `grep -m`) or a `| head` closes the pipe, and the command still
            // writing the other end (ssh, docker, ldconfig) fails the whole pipeline.
            // `awk ... exit` is the same early close: awk stops reading its input at the first record.
            for reader in ["grep -q", "grep -m", "| head", "exit }", "exit}", "exit;"] {
                assert!(!script.contains(reader), "{reader} in:\n{script}");
            }
            let mut child = Command::new("bash")
                .arg("-n")
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(script.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        // A heredoc opener inside another heredoc's body only fails inside the guest,
        // where nothing points at the generated script.
        for script in [
            install_script(&info),
            post_deploy_script(&components, false, &info),
            post_deploy_script(&components, true, &info),
            verify_script(&components, 1, OPENSTACK_RELEASE, None),
            verify_script(&components, 2, OPENSTACK_RELEASE, None),
            settle_script(None, &components),
            forget_computes_script(&components),
        ] {
            assert_heredocs_do_not_nest(&script);
        }

        // The legacy install asks for its tag, its Python and its series' constraints.
        let legacy = install_script(&releases::info("2021.1"));
        assert!(legacy.contains("kolla-ansible@12.8.0"), "{legacy}");
        assert!(legacy.contains("python=python3.9"));
        assert!(legacy.contains("constraints/upper/wallaby"));
        // Rocky 10 keeps libcrypt.so.1 in libxcrypt-compat, which uv's CPython links.
        assert!(
            legacy.contains("dnf install -y -q libxcrypt-compat"),
            "{legacy}"
        );
        // `--version` only exists from yoga's launcher on; the older tags print usage and fail.
        for release in ["2020.1", "2020.2", "2021.1", "2021.2"] {
            let script = install_script(&releases::info(release));
            assert!(
                script.contains("kolla-ansible --help >/dev/null"),
                "{release}: {script}"
            );
            assert!(
                !script.contains("kolla-ansible --version"),
                "{release}: {script}"
            );
        }
        for release in ["2022.1", "2022.2"] {
            let script = install_script(&releases::info(release));
            assert!(
                script.contains("kolla-ansible --version"),
                "{release}: {script}"
            );
            assert!(
                !script.contains("kolla-ansible --help"),
                "{release}: {script}"
            );
        }
        // A maintained branch keeps the version check and needs none of the legacy fixes.
        let current = install_script(&releases::info(OPENSTACK_RELEASE));
        assert!(current.contains("kolla-ansible --version"), "{current}");
        for absent in [
            "libxcrypt-compat",
            "Jinja2<3.1",
            "stackbox patch for this release",
        ] {
            assert!(!current.contains(absent), "{absent}: {current}");
        }

        // A restarted OVS container can leave the control's agent dead; the expansion revives it.
        assert!(RESTART_OVS_AGENT.contains("docker restart neutron_openvswitch_agent"));

        // The agent check reads the host's own agents: `--agent-type` takes names that differ
        // between client versions, and a rejected filter looked like a dead agent.
        let verify = verify_compute_script("stackbox-compute-2", true);
        assert!(verify.contains(r#"-c "Agent Type" -c Alive"#));
        assert!(!verify.contains("--agent-type"));

        // A template's containers carry its hostname; the copy drops the ones that are not its
        // own, so Kolla's OVS agent registers under this VM and not the source one.
        let wipe = wipe_template_script("stackbox-compute-2");
        assert!(wipe.contains(r#"if [ -n "$h" ] && [ "$h" != "$me" ]"#));
        assert!(wipe.contains(r#"docker rm -f "$c""#));
        // The host's own OVS containers carry the stackbox's network state, not a copy's identity.
        assert!(wipe.contains("openvswitch_db | openvswitch_vswitchd) continue"));

        // A control VM alone deploys as a multi-node stackbox: its external network is physnet2
        // on br-ex, and the computes that join later keep the same layout.
        let control_only = Topology::from_nodes(&[stackbox::StackboxNode {
            name: "dev-control".into(),
            role: Some(stackbox::Role::Control),
            index: None,
            status: "running".into(),
            memory_mib: 6656,
            ip: Some("10.254.0.10".into()),
        }])
        .unwrap();
        assert!(control_only.multinode());
        assert!(
            post_deploy_script(&components, control_only.multinode(), &info).contains("physnet2")
        );
        let alone = verify_script(&components, 0, OPENSTACK_RELEASE, None);
        assert!(alone.contains("no compute VM yet") && !alone.contains("nova-compute is not up"));

        let verify = verify_script(&components, 1, OPENSTACK_RELEASE, None);
        assert!(
            !verify.contains("ssh "),
            "all-in-one verification must not require distributed SSH keys"
        );
        assert!(verify.contains("ip netns exec \"qrouter-$router_id\""));
        assert!(verify.contains("nova-compute is not up"));
        assert!(verify.contains("cirros-0.6.3-aarch64-disk.img"));
        // The instance follows compute-1's architecture, which may differ from the control VM's.
        assert!(verify.contains("arch=${instance_arch:-$(uname -m)}"));
        assert!(verify.contains("--property hw_architecture=$arch"));
        for script in [
            install_script(&releases::info(OPENSTACK_RELEASE)),
            verify_script(&components, 1, OPENSTACK_RELEASE, None),
            verify_script(&components, 2, OPENSTACK_RELEASE, None),
            post_deploy_script(&components, false, &info),
            post_deploy_script(&components, true, &info),
        ] {
            assert!(script.contains("step \""), "a phase script reports no step");
        }
        assert!(!settle_script(None, &components).contains("endpoint set"));
        assert!(!settle_script(Some((&from, &from)), &components).contains("endpoint set"));
    }

    #[test]
    fn a_control_template_copy_forgets_only_the_computes_and_settles_without_them() {
        let components = ["keystone", "glance", "placement", "neutron", "nova"].map(String::from);
        let script = forget_computes_script(&components);
        for expected in [
            "ssh-keygen -R stackbox-compute-1 ",
            "ssh-keygen -R 10.254.0.13 ",
            "$os compute service delete \"$id\"",
            "conn.placement.delete_resource_provider(rp)",
            // A GPU stackbox's child providers hang off a compute's, and Placement refuses to
            // delete a provider with children: the reverse name order deletes them first.
            "key=lambda provider: provider.name,",
            "reverse=True,",
            "nova-manage cell_v2 delete_host --cell_uuid \"$cell\" --host \"$host\"",
            "$os network agent delete \"$id\"",
        ] {
            assert!(script.contains(expected), "{expected}");
        }
        // Nothing of the control VM's own matches.
        assert!(!script.contains("stackbox-control"));
        let without = forget_computes_script(&components[..2]);
        assert!(!without.contains("compute service") && !without.contains("network agent"));

        // The inventory test, run against the inventories it tells apart.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inventory");
        let template = "[control]\ncontrol01\n\n[network]\nnetwork01\n\n[compute]\ncompute01\n\n[monitoring]\nmonitoring01\n\n[storage]\nstorage01\n\n[deployment]\nlocalhost\n";
        let check = |inventory: &str| {
            std::fs::write(&path, inventory).unwrap();
            Command::new("bash")
                .arg("-c")
                .arg(no_compute_hosts(&path.display().to_string()))
                .status()
                .unwrap()
                .success()
        };
        let compute = config::Compute {
            name: "demo-compute-1".into(),
            hostname: "stackbox-compute-1".into(),
            address: "10.254.0.11".into(),
        };
        assert!(check(
            &config::multinode_inventory(template, "stackbox-control", &[], &BTreeMap::new())
                .unwrap()
        ));
        assert!(!check(
            &config::multinode_inventory(
                template,
                "stackbox-control",
                &[compute],
                &BTreeMap::new()
            )
            .unwrap()
        ));
        assert!(!check("[control]\nlocalhost\n\n[compute]\nlocalhost\n"));
        assert!(settle_script(None, &components).contains("no compute VM yet"));
    }

    #[test]
    fn each_published_port_maps_to_its_kolla_variable_at_its_own_offset() {
        let exposure = Exposure {
            host: "stackbox.example".into(),
            bind: [127, 0, 0, 1].into(),
            port_base: 15000,
        };
        let all: Vec<String> = crate::model::COMPONENTS
            .iter()
            .map(|c| c.id.to_owned())
            .collect();
        let urls = exposure.urls(Some(&all));
        // Every published port is listed and has a Kolla variable to be written into globals.yml.
        let ports: Vec<_> = crate::model::COMPONENTS
            .iter()
            .flat_map(|c| c.public.iter())
            .collect();
        assert_eq!(urls.len(), ports.len());
        for port in &ports {
            assert!(
                public_url_variable(port.id).is_some(),
                "{} has no kolla variable",
                port.id
            );
        }
        // Heat's API and CFN ports, and Skyline's console, keep distinct offsets.
        assert_eq!(urls["heat"], "http://stackbox.example:15009");
        assert_eq!(urls["heat-cfn"], "http://stackbox.example:15010");
        assert_eq!(urls["barbican"], "http://stackbox.example:15011");
        assert_eq!(urls["designate"], "http://stackbox.example:15012");
        assert_eq!(urls["magnum"], "http://stackbox.example:15013");
        assert_eq!(urls["skyline"], "http://stackbox.example:15014");
        assert_eq!(urls["skyline-console"], "http://stackbox.example:15015");
        assert_eq!(
            public_url_variable("heat-cfn"),
            Some("heat_cfn_public_base_endpoint")
        );
        assert_eq!(
            public_url_variable("skyline-console"),
            Some("skyline_console_public_endpoint")
        );
    }

    #[test]
    fn rebinding_moves_whole_host_port_prefixes_and_keeps_paths() {
        let exposure = |host: &str, port_base| Exposure {
            host: host.into(),
            bind: [127, 0, 0, 1].into(),
            port_base,
        };
        let map = rebind_map(
            &exposure("localhost", 15000),
            &exposure("stackbox.example", 15020),
        );
        assert_eq!(
            map["http://localhost:15000"],
            "http://stackbox.example:15020"
        );
        assert_eq!(
            map["http://localhost:15005"],
            "http://stackbox.example:15025"
        );
        assert_eq!(
            map.len(),
            crate::model::COMPONENTS
                .iter()
                .map(|c| c.public.len())
                .sum::<usize>()
        );
        // Run the generated rewrite against a stub client and globals.yml.
        let dir = tempfile::tempdir().unwrap();
        let script = settle_script(
            Some((&exposure("localhost", 15000), &exposure("localhost", 15020))),
            &["keystone".into(), "glance".into()],
        );
        let python = script
            .split("python3 - <<'PY'\n")
            .nth(1)
            .and_then(|rest| rest.split("\nPY\n").next())
            .unwrap()
            .replace("/etc/kolla", &dir.path().display().to_string())
            .replace(
                "/opt/osc/bin/openstack",
                &dir.path().join("openstack").display().to_string(),
            );
        std::fs::write(
            dir.path().join("globals.yml"),
            "keystone_public_url: \"http://localhost:15000\"\nglance_public_endpoint: \"http://localhost:15001\"\nkolla_internal_vip_address: \"10.254.0.2\"\n",
        )
        .unwrap();
        let calls = dir.path().join("calls");
        let stub = format!(
            "#!/bin/sh\necho \"$@\" >> {calls}\ncase \"$1 $2\" in\n  'endpoint list') echo '[{{\"ID\":\"k\",\"Service Name\":\"keystone\",\"URL\":\"http://localhost:15000\"}},{{\"ID\":\"n\",\"Service Name\":\"nova\",\"URL\":\"http://localhost:15005/v2.1\"}},{{\"ID\":\"o\",\"Service Name\":\"other\",\"URL\":\"http://localhost:150001\"}}]' ;;\nesac\n",
            calls = calls.display()
        );
        let client = dir.path().join("openstack");
        std::fs::write(&client, stub).unwrap();
        std::fs::set_permissions(&client, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let output = Command::new("python3")
            .args(["-c", &python])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let calls = std::fs::read_to_string(calls).unwrap();
        assert!(calls.contains("endpoint set --url http://localhost:15020 k"));
        assert!(calls.contains("endpoint set --url http://localhost:15025/v2.1 n"));
        assert!(!calls.contains(" o\n"), "{calls}");
        let globals = std::fs::read_to_string(dir.path().join("globals.yml")).unwrap();
        assert!(globals.contains("keystone_public_url: \"http://localhost:15020\""));
        assert!(globals.contains("glance_public_endpoint: \"http://localhost:15021\""));
        assert!(globals.contains("10.254.0.2"));
    }
    #[test]
    fn the_mdev_probe_tells_a_missing_type_from_a_used_one() {
        // No mtty-1 type: the compute booted the stock kernel.
        assert_eq!(mdev_probe("absent\n"), MdevProbe::Absent);
        assert_eq!(
            mdev_probe("available 24\nin-use 0\n"),
            MdevProbe::Devices {
                available: 24,
                in_use: 0
            }
        );
        // An instance already holds an mdev, so fewer are free; the type still offers 24.
        assert_eq!(
            mdev_probe("available 23\nin-use 1\n"),
            MdevProbe::Devices {
                available: 23,
                in_use: 1
            }
        );
        // Anything else is unreadable, so check_mdev refuses instead of guessing a count.
        assert_eq!(mdev_probe(""), MdevProbe::Unreadable);
        assert_eq!(mdev_probe("available 24\n"), MdevProbe::Unreadable);
        assert_eq!(mdev_probe("available\nin-use\n"), MdevProbe::Unreadable);
        // The probe travels inside ssh's single quotes, so it may not carry one itself.
        assert!(!MDEV_PROBE.contains('\''), "{MDEV_PROBE}");
        assert!(MDEV_PROBE.contains("/sys/bus/mdev/devices"));
    }
    /// The `<<'PY'` programs a verification script carries, in order.
    fn python_blocks(script: &str) -> Vec<String> {
        let mut blocks = Vec::new();
        let mut current: Option<String> = None;
        for line in script.lines() {
            match &mut current {
                None if line.contains("<<'PY'") => current = Some(String::new()),
                None => {}
                Some(block) if line.trim() == "PY" => blocks.push(current.take().unwrap()),
                Some(block) => {
                    block.push_str(line);
                    block.push('\n');
                }
            }
        }
        assert!(current.is_none(), "unterminated python heredoc");
        blocks
    }
    /// The GPU verify script installs the fake nvidia-smi through a quoted heredoc; the
    /// whole script must still be valid shell.
    #[cfg(unix)]
    #[test]
    fn the_gpu_verify_script_is_valid_shell() {
        let components = ["keystone", "glance", "placement", "neutron", "nova"].map(String::from);
        let dir = tempfile::tempdir().unwrap();
        for gpu in [
            crate::model::GpuConfig {
                mode: crate::model::GpuMode::Simulated,
                count: 2,
                ..Default::default()
            },
            crate::model::GpuConfig {
                mode: crate::model::GpuMode::Mtty,
                count: 1,
                ..Default::default()
            },
        ] {
            let script = verify_script(&components, 1, OPENSTACK_RELEASE, Some(&gpu));
            // The verification instance gets the same installer a user would pass.
            assert!(script.contains("--user-data \"$user_data\""), "{script}");
            assert!(script.contains("nvsmi_device_v11.dtd"), "{script}");
            assert!(script.contains("<nvidia_smi_log>"), "{script}");
            let path = dir.path().join("verify.sh");
            std::fs::write(&path, &script).unwrap();
            let status = std::process::Command::new("sh")
                .arg("-n")
                .arg(&path)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "the {gpu:?} verify script is not valid shell"
            );
        }
    }

    /// Every `<<'PY'` program of a GPU stackbox's verify script must still parse as Python after
    /// `format!` has turned `{{` into `{`.
    #[test]
    fn the_gpu_verify_script_holds_valid_python() {
        let components = ["keystone", "glance", "placement", "neutron", "nova"].map(String::from);
        for gpu in [
            crate::model::GpuConfig {
                mode: crate::model::GpuMode::Simulated,
                count: 4,
                ..Default::default()
            },
            crate::model::GpuConfig {
                mode: crate::model::GpuMode::Mtty,
                count: 2,
                ..Default::default()
            },
        ] {
            let script = verify_script(&components, 2, OPENSTACK_RELEASE, Some(&gpu));
            let blocks = python_blocks(&script);
            // The simulated script advertises, checks and allocates; mtty checks and allocates.
            let expected = match gpu.mode {
                crate::model::GpuMode::Simulated => 3,
                crate::model::GpuMode::Mtty => 2,
            };
            assert_eq!(blocks.len(), expected, "{script}");
            for block in blocks {
                assert_python_parses(&block);
            }
        }
        // `add-compute` advertises a new compute's providers with a script of its own.
        let advertise = gpu_advertise_script(
            Some(&crate::model::GpuConfig {
                mode: crate::model::GpuMode::Simulated,
                count: 6,
                devices: Some(2),
                ..Default::default()
            }),
            &["stackbox-compute-2".to_owned()],
        );
        for block in python_blocks(&advertise) {
            assert_python_parses(&block);
        }
    }

    /// The same scripts are one shell program each; `bash -n` parses them without running
    /// anything, which catches a quoting slip before it reaches a deployment.
    #[test]
    fn the_gpu_scripts_parse_as_shell() {
        let components = ["keystone", "glance", "placement", "neutron", "nova"].map(String::from);
        let shaped = crate::model::GpuConfig {
            mode: crate::model::GpuMode::Simulated,
            count: 6,
            devices: Some(2),
            traits: Some(vec!["CUSTOM_MAAS".into(), "CUSTOM_SRIOV".into()]),
            resource_class: Some("PGPU".into()),
        };
        let scripts = [
            verify_script(&components, 2, OPENSTACK_RELEASE, Some(&shaped)),
            verify_script(
                &components,
                2,
                OPENSTACK_RELEASE,
                Some(&crate::model::GpuConfig {
                    mode: crate::model::GpuMode::Mtty,
                    count: 24,
                    ..Default::default()
                }),
            ),
            gpu_advertise_script(
                Some(&shaped),
                &[
                    "stackbox-compute-1".to_owned(),
                    "stackbox-compute-2".to_owned(),
                ],
            ),
            // The same stackbox on a release whose nova-compute carries stackbox's backport.
            verify_script(
                &components,
                2,
                "2026.1",
                Some(&crate::model::GpuConfig {
                    mode: crate::model::GpuMode::Mtty,
                    count: 2,
                    ..Default::default()
                }),
            ),
        ];
        for script in scripts {
            let output = match Command::new("bash")
                .args(["-n", "-c", script.as_str()])
                .output()
            {
                Ok(output) => output,
                Err(error) => {
                    eprintln!("bash unavailable, skipping the gpu shell test: {error}");
                    return;
                }
            };
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stderr),
                script
            );
        }
    }

    fn assert_python_parses(block: &str) {
        let mut child = match Command::new("python3")
            .args([
                "-c",
                "import sys; compile(sys.stdin.read(), 'block', 'exec')",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                eprintln!("python3 unavailable, skipping the gpu script test: {error}");
                return;
            }
        };
        child
            .stdin
            .take()
            .unwrap()
            .write_all(block.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn a_simulated_gpu_advertises_a_child_provider_per_fake_gpu() {
        let gpu = crate::model::GpuConfig {
            mode: crate::model::GpuMode::Simulated,
            count: 5,
            devices: Some(2),
            ..Default::default()
        };
        let hosts = [
            "stackbox-compute-1".to_owned(),
            "stackbox-compute-2".to_owned(),
        ];
        let script = gpu_advertise_script(Some(&gpu), &hosts);
        for expected in [
            // A provider per fake card, under the name a vGPU deployment's Nova would use.
            "stackbox-compute-1_pci_0000_84_00_0=3",
            "stackbox-compute-1_pci_0000_85_00_0=2",
            "stackbox-compute-2_pci_0000_84_00_0=3",
            "stackbox-compute-2_pci_0000_85_00_0=2",
            // The child hangs off the compute's own provider, never the root's inventory.
            "\"parent_provider_uuid\": providers[host]",
            "\"resource_provider_generation\": generation",
            "\"/resource_providers/\" + uuid + \"/inventories\"",
            "\"/resource_providers/\" + uuid + \"/traits\"",
            "\"traits\": traits",
            "\"total\": total",
            "\"max_unit\": total",
            "\"allocation_ratio\": 1.0",
            "OpenStack-API-Version\": \"placement 1.14\"",
            // A create at 1.14 answers 201 with an empty body and Location, so the uuid
            // comes from the body, the header or a name lookup, never from the body alone.
            "uuid = created.get(\"uuid\")",
            "location.rstrip(\"/\").rsplit(\"/\", 1)[-1]",
            "uuid = named(name)",
            // A provider takes only traits the trait table holds; a fresh Placement has
            // none of stackbox's CUSTOM_* names.
            "call(\"PUT\", \"/traits/\" + trait)",
            // A request stackbox can shape: class and traits the flavor will ask for.
            "python3 - VGPU \"CUSTOM_STACKBOX_VGPU\"",
            "time.sleep(5)",
        ] {
            assert!(script.contains(expected), "{expected}\n{script}");
        }
        // A stackbox that named its own contract advertises that one.
        let custom = crate::model::GpuConfig {
            mode: crate::model::GpuMode::Simulated,
            count: 1,
            traits: Some(vec!["CUSTOM_MAAS".into(), "CUSTOM_SRIOV".into()]),
            resource_class: Some("PGPU".into()),
            ..Default::default()
        };
        let script = gpu_advertise_script(Some(&custom), &hosts[..1]);
        assert!(
            script.contains("python3 - PGPU \"CUSTOM_MAAS,CUSTOM_SRIOV\""),
            "{script}"
        );
        assert!(
            script.contains("stackbox-compute-1_pci_0000_84_00_0=1"),
            "{script}"
        );
        // The mediated device's providers are Nova's; stackbox advertises nothing for them.
        assert!(
            gpu_advertise_script(
                Some(&crate::model::GpuConfig {
                    mode: crate::model::GpuMode::Mtty,
                    count: 1,
                    ..Default::default()
                }),
                &hosts
            )
            .is_empty()
        );
        assert!(gpu_advertise_script(None, &hosts).is_empty());
    }

    #[test]
    fn a_gpu_stackbox_verifies_its_providers_flavor_and_allocation() {
        let components = ["keystone", "glance", "placement", "neutron", "nova"].map(String::from);
        let simulated = crate::model::GpuConfig {
            mode: crate::model::GpuMode::Simulated,
            count: 4,
            ..Default::default()
        };
        let script = verify_script(&components, 1, OPENSTACK_RELEASE, Some(&simulated));
        for expected in [
            // Advertised first, then checked: both name the child providers of the stackbox.
            "GPU(simulated) Placement 리소스 프로바이더 광고",
            "stackbox-compute-1_pci_0000_84_00_0=4",
            "GPU(simulated) 자식 리소스 프로바이더 확인",
            "\"parent_provider_uuid\"",
            "carries %s on its root provider",
            "lacks trait(s)",
            // The flavor asks for the standard class and the trait; the instance's
            // allocation must land on a provider this stackbox advertised.
            "stackbox.vgpu",
            "resources:VGPU=1",
            "trait:CUSTOM_STACKBOX_VGPU=required",
            "$os flavor set stackbox.vgpu",
            "which this stackbox did not advertise",
            "$work/gpu-providers",
            "/allocations/",
            // An oversubscribed request is refused by the scheduler, with the fault to show
            // it came from placement rather than from anywhere else.
            "GPU(simulated) 재고를 넘는 요청",
            "--property resources:VGPU=5",
            "grep \"No valid host\" >/dev/null",
            "was refused with NoValidHost",
        ] {
            assert!(script.contains(expected), "{expected}");
        }
        // Only the mediated device has a libvirt hostdev to look for.
        assert!(!script.contains("type='mdev'"));
        // A stackbox that asked for no trait writes none into its flavor.
        let bare = crate::model::GpuConfig {
            mode: crate::model::GpuMode::Simulated,
            count: 1,
            traits: Some(vec![]),
            ..Default::default()
        };
        let script = verify_script(&components, 1, OPENSTACK_RELEASE, Some(&bare));
        assert!(script.contains("--property resources:VGPU=1 stackbox.vgpu"));
        assert!(!script.contains("trait:CUSTOM_STACKBOX_VGPU"));

        let mtty = crate::model::GpuConfig {
            mode: crate::model::GpuMode::Mtty,
            count: 24,
            ..Default::default()
        };
        let script = verify_script(&components, 2, OPENSTACK_RELEASE, Some(&mtty));
        for expected in [
            "GPU(mtty)",
            "stackbox.vgpu",
            "resources:VGPU=1",
            "stackbox-compute-1,stackbox-compute-2 \"$placement\"",
            "stackbox-compute-1 stackbox-compute-2",
            "$os flavor set stackbox.vgpu",
            "type='mdev'",
            "docker exec nova_libvirt",
            "inventory on %d compute resource(s)",
        ] {
            assert!(script.contains(expected), "{expected}");
        }
        // The mediated device's inventory is Nova's, so stackbox neither advertises nor
        // oversubscribes it; the flavor carries no trait the driver does not set.
        assert!(
            !script.contains("Placement 리소스 프로바이더 광고"),
            "{script}"
        );
        assert!(!script.contains("stackbox.vgpu-full"), "{script}");
        assert!(!script.contains("trait:VGPU"));
        // 2026.2 is the first release whose nova has the mtty support itself, so this stackbox's
        // container needs no backport and the verification asks for none.
        assert!(!script.contains("백포트"), "{script}");

        // On an older release stackbox patches nova-compute at start; the verification runs the
        // same hook in `--check` mode on every compute before it boots an instance.
        let backported = verify_script(&components, 2, "2026.1", Some(&mtty));
        for expected in [
            "nova-compute의 Nova mtty 백포트 확인 (2026.1)",
            "docker exec nova_compute sha256sum /var/lib/kolla/venv/lib/python3.12/site-packages/nova/virt/libvirt/config.py /var/lib/kolla/venv/lib/python3.12/site-packages/nova/virt/libvirt/driver.py",
            "sha256sum /etc/stackbox/nova-mtty/2026.1/nova/virt/libvirt/config.py /etc/stackbox/nova-mtty/2026.1/nova/virt/libvirt/driver.py",
            "for host in stackbox-compute-1 stackbox-compute-2; do",
            "nova-compute is not running with stackbox's Nova mtty backport files",
        ] {
            assert!(backported.contains(expected), "{expected}");
        }
        // It is a stackbox's own check: a release stackbox does not patch (2025.1 is patched too,
        // and a simulated GPU never is) must not claim one.
        assert!(
            !verify_script(&components, 2, "2025.1", Some(&simulated)).contains("백포트"),
            "{script}"
        );
        assert!(!verify_script(&components, 2, "2026.1", None).contains("백포트"));
        // A stackbox without a GPU verifies no GPU.
        let plain = verify_script(&components, 1, OPENSTACK_RELEASE, None);
        assert!(!plain.contains("stackbox.vgpu"));
        assert!(!plain.contains("GPU("));
    }

    /// The `bound=''` block of the mtty check, which asks each compute for its domains' XML.
    fn mtty_check(script: &str) -> String {
        let lines: Vec<&str> = script.lines().collect();
        let start = lines
            .iter()
            .position(|line| *line == "bound=''")
            .unwrap_or_else(|| panic!("no mtty check in {script}"));
        let end = lines
            .iter()
            .position(|line| line.starts_with("echo \"mtty mdev is bound"))
            .unwrap_or_else(|| panic!("no mtty check in {script}"));
        assert!(start < end, "{script}");
        lines[start..=end].join("\n")
    }

    /// Runs the mtty check with a stand-in `ssh` on PATH.
    fn run_mtty_check(check: &str, ssh: &str) -> std::process::Output {
        let dir = tempfile::tempdir().unwrap();
        let stand_in = dir.path().join("ssh");
        std::fs::write(&stand_in, ssh).unwrap();
        std::fs::set_permissions(
            &stand_in,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let mut paths = vec![dir.path().to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        Command::new("bash")
            .args(["-c", &format!("set -euo pipefail\n{check}")])
            .env("PATH", std::env::join_paths(paths).unwrap())
            .output()
            .unwrap()
    }

    /// The compute and agent waits poll OpenStack for minutes. A failing `openstack` must not be
    /// hidden behind `|| true`: the timeout names the last error, and a healthy answer still
    /// passes. The interval is shortened here; the loop count and the script's interval stay.
    #[test]
    fn the_compute_wait_reports_the_last_openstack_error() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join("admin-openrc.sh");
        std::fs::write(&rc, "").unwrap();
        let fake = dir.path().join("openstack");
        std::fs::write(
            &fake,
            r#"#!/bin/bash
case "$*" in
  *"compute service list"*)
    [ "$FAKE_FAIL" = compute ] && { echo "Missing value auth-url required" >&2; exit 1; }
    echo up ;;
  *"network agent list"*)
    [ "$FAKE_FAIL" = network ] && { echo "Unable to establish connection to https://keystone:5000" >&2; exit 1; }
    echo "Open vSwitch True" ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let script = verify_compute_script("stackbox-compute-1", true)
            .replace("/etc/kolla/admin-openrc.sh", rc.to_str().unwrap())
            .replace("/opt/osc/bin/openstack", fake.to_str().unwrap())
            .replace("sleep 5", "sleep 0.01");
        let run = |fail: Option<&str>| {
            let mut cmd = Command::new("bash");
            cmd.arg("-c").arg(&script).env_remove("FAKE_FAIL");
            if let Some(fail) = fail {
                cmd.env("FAKE_FAIL", fail);
            }
            cmd.output().unwrap()
        };

        let output = run(None);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains("nova-compute stackbox-compute-1 up"),
            "{stdout}"
        );
        assert!(
            stdout.contains("Open vSwitch agent on stackbox-compute-1 alive"),
            "{stdout}"
        );

        let output = run(Some("compute"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(
            stderr.contains(
                "nova-compute stackbox-compute-1 is not up: missing (last error: Missing value auth-url required)"
            ),
            "{stderr}"
        );

        let output = run(Some("network"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(
            stderr.contains(
                "Open vSwitch agent on stackbox-compute-1 is not alive: missing (last error: Unable to establish connection to https://keystone:5000)"
            ),
            "{stderr}"
        );
    }

    /// The wait loops keep their error file until the script ends. A run that dies inside the
    /// loop (here `set -e` stops it at the pause after a failing openstack call) must not leave
    /// that file behind. `mktemp` ignores `TMPDIR` on macOS, so a stand-in `mktemp` puts the
    /// file in the test's directory.
    #[test]
    fn the_compute_wait_removes_its_error_file_when_cut_short() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join("admin-openrc.sh");
        std::fs::write(&rc, "").unwrap();
        let fake = dir.path().join("openstack");
        std::fs::write(&fake, "#!/bin/bash\necho boom >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let made = dir.path().join("made");
        std::fs::create_dir(&made).unwrap();
        let mktemp = dir.path().join("mktemp");
        std::fs::write(
            &mktemp,
            format!(
                "#!/bin/bash\np='{}'/err.$$\n: > \"$p\"\necho \"$p\"\n",
                made.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&mktemp, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let script = verify_compute_script("stackbox-compute-1", false)
            .replace("/etc/kolla/admin-openrc.sh", rc.to_str().unwrap())
            .replace("/opt/osc/bin/openstack", fake.to_str().unwrap())
            .replace("sleep 5", "false");
        let path = std::env::join_paths(std::iter::once(dir.path().to_path_buf()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        let output = Command::new("bash")
            .arg("-c")
            .arg(&script)
            .env("PATH", path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let left: Vec<_> = std::fs::read_dir(&made)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "error files left behind: {left:?}");
    }

    /// The check runs under `set -euo pipefail` and reads libvirt's XML over ssh. A reader that
    /// stops at its first match (`grep -q`) closes the pipe while ssh is still writing the other
    /// domains, and ssh's 255 then fails the pipeline through `pipefail` although the mdev is
    /// bound: with two stackbox guests up, `deploy --from verify` failed 8 times in 10.
    #[test]
    fn the_mtty_check_reads_the_libvirt_xml_to_the_end() {
        let components = ["keystone", "glance", "placement", "neutron", "nova"].map(String::from);
        let script = verify_script(
            &components,
            2,
            OPENSTACK_RELEASE,
            Some(&crate::model::GpuConfig {
                mode: crate::model::GpuMode::Mtty,
                count: 24,
                ..Default::default()
            }),
        );
        let check = mtty_check(&script);
        // A stand-in ssh writes a domain that carries an mdev hostdev and then keeps writing
        // the rest, like `virsh dumpxml` of several domains. Leaving early leaves it writing
        // into a closed pipe, which ssh reports as 255.
        let output = run_mtty_check(
            &check,
            r#"trap 'exit 255' PIPE
printf '%s\n' "<domain type='kvm'>" "<devices>"
printf '%s\n' "  <hostdev mode='subsystem' type='mdev' managed='no' model='vfio-pci' display='off'>"
printf '%s\n' "    <source><address uuid='7166abcc-0000-0000-0000-000000000000'/></source>"
printf '%s\n' "  </hostdev></devices></domain>"
# The check has its match by now; a reader that left closes the pipe here.
sleep 0.3
for _ in $(seq 200); do printf '%s\n' "  <interface type='network'/>"; done
"#,
        );
        assert!(
            output.status.success(),
            "the mtty check rejected a domain that carries an mdev hostdev: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(
                "mtty mdev is bound to the instance's libvirt domain on stackbox-compute-1"
            ),
            "{stdout}"
        );
        // A domain without an mdev hostdev still fails the check, so the fix stays a check.
        let output = run_mtty_check(&check, "printf '%s\\n' \"<domain><devices/></domain>\"\n");
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(
                "no libvirt domain on stackbox-compute-1 stackbox-compute-2 has an mdev hostdev"
            ),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Answer the inventory block's Placement requests: the provider list, then the
    /// inventories of each provider it asks about.
    fn placement_server(providers: &'static str, inventories: &'static str) -> String {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut path = String::new();
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if let Some(rest) = line.strip_prefix("GET ") {
                        path = rest.split_whitespace().next().unwrap_or("").to_string();
                    }
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    line.clear();
                }
                let body = if path.starts_with("/resource_providers/") {
                    inventories
                } else {
                    providers
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        format!("http://{addr}")
    }

    /// The inventory line is read as the stackbox's capacity, so it names the class total a
    /// compute offers. Reporting the requested count made a 24-device kernel look like 1.
    #[test]
    fn the_gpu_inventory_log_names_the_class_total_not_the_request() {
        let components = ["keystone", "glance", "placement", "neutron", "nova"].map(String::from);
        let script = verify_script(
            &components,
            1,
            OPENSTACK_RELEASE,
            Some(&crate::model::GpuConfig {
                mode: crate::model::GpuMode::Mtty,
                count: 1,
                ..Default::default()
            }),
        );
        let block = python_blocks(&script).remove(0);
        // Nova discovers the kernel's mdev type as a child provider of the compute, whose
        // inventory covers all 24 devices however many this stackbox requested.
        let placement = placement_server(
            r#"{"resource_providers":[{"uuid":"p1","name":"stackbox-compute-1_mtty_mtty"}]}"#,
            r#"{"inventories":{"VGPU":{"total":24}}}"#,
        );
        let mut child = match Command::new("python3")
            .arg("-")
            .args(["VGPU", "1", "stackbox-compute-1", &placement, "token"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                eprintln!("python3 unavailable, skipping the gpu script test: {error}");
                return;
            }
        };
        child
            .stdin
            .take()
            .unwrap()
            .write_all(block.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "VGPU inventory on 1 compute resource(s): 24 each"
        );
    }

    /// A stateful Placement stand-in (`tests/fixtures/fake-placement.py`) with the response
    /// shapes a real one produces, so a test can run the generated Python block itself.
    struct FakePlacement {
        child: std::process::Child,
        url: String,
    }

    impl FakePlacement {
        /// `None` when python3 is not installed, which the Python tests already skip on.
        fn start() -> Option<Self> {
            let mut child = Command::new("python3")
                .args([
                    "-u",
                    "-c",
                    include_str!("../../tests/fixtures/fake-placement.py"),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let mut url = String::new();
            std::io::BufRead::read_line(
                &mut std::io::BufReader::new(child.stdout.as_mut()?),
                &mut url,
            )
            .ok()?;
            let url = url.trim().to_owned();
            url.starts_with("http://").then_some(Self { child, url })
        }

        /// Everything the script wrote, as the fixture reports it.
        fn state(&self) -> serde_json::Value {
            use std::io::Read as _;
            let host_path = self.url.trim_start_matches("http://");
            let (host, _) = host_path.split_once('/').unwrap_or((host_path, ""));
            let mut stream = std::net::TcpStream::connect(host).unwrap();
            write!(
                stream,
                "GET /_state HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).unwrap();
            let body = answer.split_once("\r\n\r\n").map(|(_, body)| body);
            serde_json::from_str(body.unwrap_or_default()).unwrap()
        }
    }

    impl Drop for FakePlacement {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Run one `<<'PY'` program of a generated script, as the shell would.
    fn run_python_block(block: &str, args: &[&str]) -> std::process::Output {
        let mut child = Command::new("python3")
            .arg("-")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("python3 runs the GPU script block");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(block.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn simulated_gpu() -> crate::model::GpuConfig {
        crate::model::GpuConfig {
            mode: crate::model::GpuMode::Simulated,
            count: 2,
            traits: Some(vec![
                "CUSTOM_STACKBOX_VGPU".into(),
                "CUSTOM_E2E_EXTRA".into(),
            ]),
            ..Default::default()
        }
    }

    /// The advertise block runs against Placement 1.14: creating a provider answers 201 with
    /// an empty body, and a provider takes only traits the trait table holds. Reading
    /// `created["uuid"]` and skipping the trait table failed a simulated deploy.
    #[cfg(unix)]
    #[test]
    fn a_simulated_gpu_advertise_script_creates_its_traits_and_finds_a_new_provider() {
        let gpu = simulated_gpu();
        let hosts = ["stackbox-compute-1".to_owned()];
        let block = python_blocks(&gpu_advertise_script(Some(&gpu), &hosts)).remove(0);
        let Some(placement) = FakePlacement::start() else {
            eprintln!("python3 unavailable, skipping the gpu advertise test");
            return;
        };
        let provider = "stackbox-compute-1_pci_0000_84_00_0";
        let spec = format!("stackbox-compute-1:{provider}=2");
        for run in 0..2 {
            let output = run_python_block(
                &block,
                &[
                    "VGPU",
                    "CUSTOM_STACKBOX_VGPU,CUSTOM_E2E_EXTRA",
                    &placement.url,
                    "token",
                    &spec,
                ],
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "run {run}: {stderr}");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains(&format!(
                    "{provider}: VGPU=2, traits CUSTOM_STACKBOX_VGPU,CUSTOM_E2E_EXTRA"
                )),
                "run {run}: {stdout}"
            );
            if run == 0 {
                assert!(
                    stdout.contains(&format!(
                        "{provider}: created as a child of stackbox-compute-1"
                    )),
                    "{stdout}"
                );
            }
        }
        // The provider exists, carries the inventory and both traits, and the second run
        // reused it instead of failing on the name it had just created.
        let state = placement.state();
        assert_eq!(
            state["traits"],
            serde_json::json!(["CUSTOM_E2E_EXTRA", "CUSTOM_STACKBOX_VGPU"])
        );
        let providers = state["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 2, "{state}");
        let child = providers.iter().find(|p| p["name"] == provider).unwrap();
        assert_eq!(
            child["parent_provider_uuid"],
            "11111111-1111-1111-1111-111111111111"
        );
        assert_eq!(child["inventories"]["VGPU"]["total"], 2);
        assert_eq!(
            child["traits"],
            serde_json::json!(["CUSTOM_STACKBOX_VGPU", "CUSTOM_E2E_EXTRA"])
        );
        assert_eq!(state["microversions"], serde_json::json!(["1.14"]));
    }

    /// The inventory check reads a provider's trait list, which Placement answers as strings
    /// on every microversion; reading `trait["name"]` failed the check after the inventory
    /// was right. A trait the provider does not carry still fails it.
    #[cfg(unix)]
    #[test]
    fn a_simulated_gpu_inventory_check_reads_traits_as_strings_and_fails_on_a_missing_one() {
        let gpu = simulated_gpu();
        let hosts = ["stackbox-compute-1".to_owned()];
        let Some(placement) = FakePlacement::start() else {
            eprintln!("python3 unavailable, skipping the gpu inventory test");
            return;
        };
        let provider = "stackbox-compute-1_pci_0000_84_00_0";
        let spec = format!("stackbox-compute-1:{provider}=2");
        // A deploy advertises first, so the check has something to read.
        let advertise = python_blocks(&gpu_advertise_script(Some(&gpu), &hosts)).remove(0);
        let output = run_python_block(
            &advertise,
            &[
                "VGPU",
                "CUSTOM_STACKBOX_VGPU,CUSTOM_E2E_EXTRA",
                &placement.url,
                "token",
                &spec,
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("gpu-providers");
        let check = python_blocks(&simulated_inventory_check(&gpu, &hosts)).remove(0);
        let output = run_python_block(
            &check,
            &[
                "VGPU",
                "CUSTOM_STACKBOX_VGPU,CUSTOM_E2E_EXTRA",
                out.to_str().unwrap(),
                &placement.url,
                "token",
                &spec,
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "VGPU on 1 child provider(s) of 1 compute(s): 0000_84_00_0=2"
        );
        let lines = std::fs::read_to_string(&out).unwrap();
        assert_eq!(lines.lines().count(), 1, "{lines}");
        assert!(lines.starts_with(provider), "{lines}");
        // A trait the provider does not carry is still a failure.
        let stricter = crate::model::GpuConfig {
            traits: Some(vec!["CUSTOM_STACKBOX_VGPU".into(), "CUSTOM_EXTRA_2".into()]),
            ..gpu
        };
        let check = python_blocks(&simulated_inventory_check(&stricter, &hosts)).remove(0);
        let output = run_python_block(
            &check,
            &[
                "VGPU",
                "CUSTOM_STACKBOX_VGPU,CUSTOM_EXTRA_2",
                out.to_str().unwrap(),
                &placement.url,
                "token",
                &spec,
            ],
        );
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("lacks trait(s) CUSTOM_EXTRA_2"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn guest_line(bdf: &str, driver: &str, group: &str, access: &str) -> String {
        format!("device {bdf} 8086 1521 {driver} {group} {access}\n")
    }

    fn healthy_probe(devices: &str) -> String {
        format!("{devices}allow_unsafe_interrupts Y\nvfio_container yes\n")
    }

    #[test]
    fn the_guest_pci_probe_accepts_bound_devices_with_their_groups() {
        let output = healthy_probe(&format!(
            "{}{}",
            guest_line("0000:00:05.0", "vfio-pci", "3", "yes"),
            guest_line("0000:00:06.0", "vfio-pci", "4", "yes"),
        ));
        let devices = parse_guest_pci_probe(&output, 2).unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].bdf, "0000:00:05.0");
        assert_eq!(devices[0].vendor_id, "8086");
        assert_eq!(devices[0].product_id, "1521");
        assert_eq!(devices[0].iommu_group, 3);
        assert_eq!(devices[1].iommu_group, 4);
        // More devices than the stackbox asked for is fine.
        assert_eq!(parse_guest_pci_probe(&output, 1).unwrap().len(), 2);
    }

    #[test]
    fn the_guest_pci_probe_names_a_device_not_bound_to_vfio_pci() {
        let output = healthy_probe(&guest_line("0000:00:05.0", "xhci_hcd", "3", "yes"));
        let error = parse_guest_pci_probe(&output, 1).unwrap_err().to_string();
        assert!(error.contains("0000:00:05.0"), "{error}");
        assert!(error.contains("xhci_hcd"), "{error}");
        assert!(error.contains("vfio-pci"), "{error}");
        // Not bound at all is the same refusal.
        let unbound = healthy_probe(&guest_line("0000:00:05.0", "-", "3", "yes"));
        assert!(parse_guest_pci_probe(&unbound, 1).is_err());
    }

    #[test]
    fn the_guest_pci_probe_names_a_device_without_an_iommu_group() {
        let output = healthy_probe(&guest_line("0000:00:05.0", "vfio-pci", "-", "no"));
        let error = parse_guest_pci_probe(&output, 1).unwrap_err().to_string();
        assert!(error.contains("0000:00:05.0"), "{error}");
        assert!(error.contains("no IOMMU group"), "{error}");
    }

    #[test]
    fn the_guest_pci_probe_names_a_group_without_its_vfio_node() {
        let output = healthy_probe(&guest_line("0000:00:05.0", "vfio-pci", "3", "no"));
        let error = parse_guest_pci_probe(&output, 1).unwrap_err().to_string();
        assert!(error.contains("/dev/vfio/3"), "{error}");
    }

    #[test]
    fn the_guest_pci_probe_refuses_interrupts_not_allowed_and_a_missing_container() {
        let line = guest_line("0000:00:05.0", "vfio-pci", "3", "yes");
        let closed = format!("{line}allow_unsafe_interrupts N\nvfio_container yes\n");
        let error = parse_guest_pci_probe(&closed, 1).unwrap_err().to_string();
        assert!(error.contains("allow_unsafe_interrupts"), "{error}");
        let absent = format!("{line}allow_unsafe_interrupts missing\nvfio_container yes\n");
        assert!(parse_guest_pci_probe(&absent, 1).is_err());
        let no_container = format!("{line}allow_unsafe_interrupts Y\nvfio_container no\n");
        let error = parse_guest_pci_probe(&no_container, 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("/dev/vfio/vfio"), "{error}");
    }

    #[test]
    fn the_guest_pci_probe_refuses_fewer_devices_than_the_stackbox_passes_through() {
        let output = healthy_probe(&guest_line("0000:00:05.0", "vfio-pci", "3", "yes"));
        let error = parse_guest_pci_probe(&output, 2).unwrap_err().to_string();
        assert!(error.contains("1 PCI devices"), "{error}");
        assert!(error.contains("2 passed through"), "{error}");
        assert!(parse_guest_pci_probe(&healthy_probe(""), 1).is_err());
    }

    #[test]
    fn the_guest_pci_probe_refuses_a_malformed_line() {
        let output = healthy_probe("device 0000:00:05.0 8086\n");
        assert!(parse_guest_pci_probe(&output, 0).is_err());
    }

    #[test]
    fn the_pci_probe_is_valid_shell_without_single_quotes() {
        // It crosses ssh inside single quotes when the compute is on another host.
        assert!(!PCI_PROBE.contains('\''), "{PCI_PROBE}");
        assert!(!PCI_PROBE.contains("grep -q"), "{PCI_PROBE}");
        assert!(!PCI_PROBE.contains("| head"), "{PCI_PROBE}");
        let Ok(output) = Command::new("bash").args(["-n", "-c", PCI_PROBE]).output() else {
            eprintln!("bash unavailable, skipping the PCI probe syntax test");
            return;
        };
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A deployed stackbox that already runs mtty gains PCI: its nova-compute.conf keeps the mtty
    /// sections and adds `[pci]`, and an mtty-free one gets `[pci]` alone. The nova roles the
    /// reconfigure touches are the control VM's and compute-1's, and no other compute's.
    #[test]
    fn attaching_pci_keeps_the_mtty_sections_and_reconfigures_control_and_compute_1() {
        let devices = pci_devices_for_verify();
        let with_mtty = config::nova_compute_conf(true, &devices);
        assert!(
            with_mtty.contains("enabled_mdev_types = mtty-1"),
            "{with_mtty}"
        );
        assert!(
            with_mtty.contains("resume_guests_state_on_host_boot = True"),
            "{with_mtty}"
        );
        assert!(with_mtty.contains("\n[pci]\n"), "{with_mtty}");
        assert!(
            with_mtty.contains("\"address\": \"0000:00:05.0\""),
            "{with_mtty}"
        );
        let plain = config::nova_compute_conf(false, &devices);
        assert!(!plain.contains("mtty"), "{plain}");
        assert!(plain.contains("\n[pci]\n"), "{plain}");
        // The scheduler and the aliases the control VM's Nova services read.
        let global = config::nova_global_conf(&devices).unwrap();
        assert!(global.contains("PciPassthroughFilter"), "{global}");
        assert!(global.contains("[pci]\nalias = "), "{global}");
        // The reconfigure is limited to the control VM and compute-1, and only the nova tags.
        let info = releases::info(OPENSTACK_RELEASE);
        let command = kolla_ansible_nova_reconfigure(&info);
        assert!(command.contains("kolla-ansible reconfigure"), "{command}");
        assert!(
            command.ends_with("--limit stackbox-control,stackbox-compute-1 --tags nova"),
            "{command}"
        );
        assert!(
            !command.contains("--limit stackbox-compute-1 "),
            "{command}"
        );
    }

    fn pci_devices_for_verify() -> Vec<GuestPciDevice> {
        vec![
            GuestPciDevice {
                bdf: "0000:00:05.0".into(),
                vendor_id: "10de".into(),
                product_id: "1eb8".into(),
                iommu_group: 3,
            },
            GuestPciDevice {
                bdf: "0000:00:06.0".into(),
                vendor_id: "8086".into(),
                product_id: "1521".into(),
                iommu_group: 4,
            },
        ]
    }

    #[test]
    fn the_pci_verify_script_creates_the_flavors_and_reads_the_domain_xml() {
        let script = pci_verify_script("stackbox-compute-1", &pci_devices_for_verify());
        assert!(
            script.contains("flavor create --ram 512 --disk 1 --vcpus 1 stackbox.pci "),
            "{script}"
        );
        assert!(
            script.contains("\"pci_passthrough:alias=stackbox-pci:1\""),
            "{script}"
        );
        // A second device pair gets its own flavor and alias.
        assert!(script.contains("stackbox.pci-2"), "{script}");
        assert!(
            script.contains("\"pci_passthrough:alias=stackbox-pci-2:1\""),
            "{script}"
        );
        assert!(script.contains("--flavor stackbox.pci "), "{script}");
        assert!(script.contains("virsh dumpxml $inst"), "{script}");
        assert!(
            script.contains("<hostdev mode='subsystem' type='pci'"),
            "{script}"
        );
        assert!(script.contains("server delete --wait"), "{script}");
        assert!(script.contains("stackbox-pci-verify-"), "{script}");
        // One pair: no second flavor.
        let single = pci_verify_script("stackbox-compute-1", &pci_devices_for_verify()[..1]);
        assert!(!single.contains("stackbox.pci-2"), "{single}");
    }

    #[test]
    fn the_pci_verify_script_is_safe_under_pipefail() {
        let script = pci_verify_script("stackbox-compute-1", &pci_devices_for_verify());
        assert!(!script.contains("grep -q"), "{script}");
        assert!(!script.contains("| head"), "{script}");
        assert!(!script.contains("grep -m"), "{script}");
        let Ok(output) = Command::new("bash").args(["-n", "-c", &script]).output() else {
            eprintln!("bash unavailable, skipping the PCI verify syntax test");
            return;
        };
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The `pci_host=` to the success echo of the PCI check, which reads the domain's XML.
    fn pci_check(script: &str) -> String {
        let lines: Vec<&str> = script.lines().collect();
        let start = lines
            .iter()
            .position(|line| line.starts_with("pci_host="))
            .unwrap_or_else(|| panic!("no PCI check in {script}"));
        let end = lines
            .iter()
            .position(|line| line.starts_with("echo \"PCI hostdev is bound"))
            .unwrap_or_else(|| panic!("no PCI check in {script}"));
        assert!(start < end, "{script}");
        let mut check = String::from("inst=instance-00000001\n");
        check.push_str(&lines[start..=end].join("\n"));
        check
    }

    /// Like `the_mtty_check_reads_the_libvirt_xml_to_the_end`: a stand-in ssh writes the domain
    /// with its hostdev and keeps writing the rest. The XML is read whole, so ssh ends 0.
    #[test]
    fn the_pci_check_reads_the_libvirt_xml_to_the_end() {
        let script = pci_verify_script("stackbox-compute-1", &pci_devices_for_verify());
        let check = pci_check(&script);
        let output = run_mtty_check(
            &check,
            r#"trap 'exit 255' PIPE
printf '%s\n' "<domain type='kvm'>" "<devices>"
printf '%s\n' "  <hostdev mode='subsystem' type='pci' managed='yes'>"
printf '%s\n' "    <source><address domain='0x0000' bus='0x01' slot='0x00' function='0x0'/></source>"
printf '%s\n' "  </hostdev></devices></domain>"
sleep 0.3
for _ in $(seq 200); do printf '%s\n' "  <interface type='network'/>"; done
"#,
        );
        assert!(
            output.status.success(),
            "the PCI check rejected a domain with a PCI hostdev: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(
                "PCI hostdev is bound to the instance's libvirt domain on stackbox-compute-1"
            ),
            "{stdout}"
        );
        // A domain with only a mdev or no hostdev fails the check.
        let output = run_mtty_check(
            &check,
            "printf '%s\\n' \"<domain type='kvm'><devices><hostdev mode='subsystem' type='mdev'/></devices></domain>\"\n",
        );
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(
                "libvirt domain instance-00000001 on stackbox-compute-1 has no PCI hostdev"
            ),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
