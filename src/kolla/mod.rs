mod config;
pub mod releases;

use std::fmt;

use anyhow::{Context, Result};
use microsandbox::Sandbox;

use crate::farm;
use crate::model::Exposure;
use crate::shell::{run, stream};
pub use config::OPENSTACK_RELEASE;
/// Python the golden image ships, which the releases that still have a branch run on.
pub const GUEST_PYTHON: &str = "3.12";
use config::{Compute, Topology};
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
const LOG_DIR: &str = "/var/log/farm";
/// Neutron's external network lives behind the all-in-one host's `farm-extgw` veth.
const EXTERNAL_NETWORK: &str = "172.24.4.0/24";
const EXTERNAL_GATEWAY: &str = "172.24.4.1";
/// Replaces the vswitchd image's start hook, which loads the openvswitch kernel module
/// under `set -e`. The userspace datapath needs no module, and the guest kernel has none.
const OVS_EXTEND_START: &str = "/etc/farm/ovs-extend-start";
const OVS_EXTEND_START_SCRIPT: &str = r#"#!/bin/bash
# Written by farm: kolla's version also runs `modprobe openvswitch`.
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

pub async fn deploy(farm_id: &str, from: Phase, components: &[String]) -> Result<()> {
    deploy_with(
        farm_id,
        from,
        None,
        components,
        OPENSTACK_RELEASE,
        Arc::new(|_| Ok(())),
    )
    .await
}

/// `components` is the farm plan's resolved service list.
pub async fn deploy_with(
    farm_id: &str,
    from: Phase,
    exposure: Option<Exposure>,
    components: &[String],
    release: &str,
    sink: EventSink,
) -> Result<()> {
    deploy_on(
        farm_id,
        from,
        exposure,
        components,
        release,
        &[],
        None,
        sink,
    )
    .await
}

/// Deploy a farm that spans hosts from its control VM's host. The compute VMs numbered in
/// `remote` run on other hosts, which [`prepare_share`] readied; Kolla reaches every compute
/// with `ssh_private_key`.
pub async fn deploy_spanning(
    farm_id: &str,
    from: Phase,
    exposure: Option<Exposure>,
    components: &[String],
    release: &str,
    remote: &[u8],
    ssh_private_key: &str,
    sink: EventSink,
) -> Result<()> {
    deploy_on(
        farm_id,
        from,
        exposure,
        components,
        release,
        remote,
        Some(ssh_private_key),
        sink,
    )
    .await
}

async fn deploy_on(
    farm_id: &str,
    from: Phase,
    exposure: Option<Exposure>,
    components: &[String],
    release: &str,
    remote: &[u8],
    ssh_private_key: Option<&str>,
    sink: EventSink,
) -> Result<()> {
    let mut nodes = farm::get_farm(farm_id).await?;
    // Computes on other hosts take their place in the layout; they are reached over SSH only.
    nodes.extend(remote.iter().map(|index| farm::FarmNode {
        name: String::new(),
        role: Some(farm::Role::Compute),
        index: Some(*index),
        status: "running".into(),
        memory_mib: 0,
        ip: None,
    }));
    let mut topology = Topology::from_nodes(&nodes)?;
    let deployer = farm::connect(&topology.name).await?;
    let mut computes = Vec::new();
    let result = async {
        for compute in &topology.computes {
            let sandbox = if compute.name.is_empty() {
                None
            } else {
                Some(farm::connect(&compute.name).await?)
            };
            computes.push((compute.clone(), sandbox));
        }
        if let Some(key) = ssh_private_key {
            install_ssh_key(&deployer, key).await?;
        }
        farm::wait_until_booted(&deployer).await?;
        // Farms created with a fixed guest identity bind every service to it; older farms use eth0.
        let identity = run(&deployer, &identity_command()).await?;
        let (interface, address) = identity
            .trim()
            .split_once(' ')
            .context("cannot read the guest service address")?;
        topology.set_address(address)?;
        topology.interface = interface.to_owned();
        deploy_phases(
            farm_id,
            from,
            &deployer,
            &computes,
            &topology,
            exposure.as_ref(),
            components,
            release,
            &sink,
        )
        .await
    }
    .await;
    deployer.detach().await;
    for (_, sandbox) in computes.into_iter().filter_map(|(c, s)| Some((c, s?))) {
        sandbox.detach().await;
    }
    result
}

/// Reads the guest's service interface and address on one line: the fixed identity a farm
/// created with one binds to, or `eth0` on farms made before it.
fn identity_command() -> String {
    format!(
        "if [ -f {conf} ]; then . {conf}; echo \"{dummy} $FARM_ADDRESS\"; else echo \"eth0 $(ip -4 -o addr show dev eth0 | awk '{{split($4,a,\"/\"); print a[1]; exit}}')\"; fi",
        conf = farm::IDENTITY_CONF,
        dummy = farm::GUEST_INTERFACE,
    )
}

/// Give the control VM the key Kolla reaches the computes with.
/// Add one compute VM to a deployed farm with Kolla. `nodes` are the farm's VMs on this
/// host; computes on other hosts were prepared there. Everything is limited to the new
/// compute, so the control plane keeps running. A compute booted from a compute template
/// (`from_template`) already has Kolla's containers: its identity is wiped and Kolla only
/// reconfigures it, skipping bootstrap-servers and the image pull.
pub async fn deploy_compute(
    farm_id: &str,
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
        .find(|spec| spec.role == farm::Role::Control)
        .context("a compute can only join a farm with a control VM")?;
    let new_suffix = format!("compute-{index}");
    let new_spec = plan
        .nodes
        .iter()
        .find(|spec| spec.role == farm::Role::Compute && spec.index == Some(index))
        .with_context(|| format!("farm plan has no {new_suffix}"))?;
    let release = releases::info(plan.release.as_deref().unwrap_or(OPENSTACK_RELEASE));
    let control_name = farm::node_name(farm_id, farm::Role::Control, None);
    let control = farm::connect(&control_name).await?;
    let mut sandboxes: Vec<Sandbox> = Vec::new();
    let result = async {
        farm::wait_until_booted(&control).await?;
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
                .filter(|spec| spec.role == farm::Role::Compute)
                .map(|spec| Compute {
                    name: farm::node_name(farm_id, spec.role, spec.index),
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
        farm::update_identity(&control, control_spec, &plan.nodes).await?;
        let neutron = components.iter().any(|c| c == "neutron");
        let mut computes: Vec<(Compute, Option<Sandbox>)> = Vec::new();
        for compute in &topology.computes {
            let suffix = compute
                .hostname
                .strip_prefix("farm-")
                .context("compute hostname has no role prefix")?;
            if !nodes.iter().any(|node| node == suffix) {
                computes.push((compute.clone(), None));
                continue;
            }
            let sandbox = farm::connect(&compute.name).await?;
            if suffix == new_suffix && from_template {
                step(&sink, format!("{new_suffix}의 템플릿 identity 초기화"))?;
                farm::wait_until_booted(&sandbox).await?;
                farm::update_identity(&sandbox, new_spec, &plan.nodes).await?;
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
        step(&sink, "control VM에서 새 compute로 SSH 키 배포")?;
        distribute_ssh_key(&control, &computes).await?;
        step(&sink, "Kolla multinode inventory 갱신")?;
        write_inventory(&control, &topology).await?;
        let host = new_spec.hostname();
        if from_template {
            sink(DeployEvent::Phase(Phase::Deploy))?;
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
            step(&sink, format!("kolla-ansible deploy --limit {host}"))?;
            stream(
                &control,
                &kolla_ansible_limited("deploy", &host, &release),
                &format!("{LOG_DIR}/add-compute-{index}-deploy.log"),
                &sink,
            )
            .await?;
        }
        // Kolla restarts the control VM's OVS containers when the farm gains a compute, and the
        // agent container that starts with them can find the database not yet listening and die.
        // A compute would then join a farm whose network node has no agent, so give it one.
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
        Ok::<_, anyhow::Error>(())
    }
    .await;
    control.detach().await;
    for sandbox in sandboxes {
        sandbox.detach().await;
    }
    result
}

/// Make a farm restored from a control template forget the computes of the farm it was
/// frozen from, so its first expansion is compute-1 again. Only this copy changes; the
/// source farm keeps its computes. Kolla's inventory lists no compute, the control VM forgets
/// their SSH host keys, and OpenStack drops their registrations: each nova-compute service
/// (Nova removes its compute node, placement resource provider and cell host mapping with
/// it), any of those left behind, and their Neutron agents. Waits for the APIs first, and
/// is safe to run again.
pub async fn forget_computes(farm_id: &str, components: &[String], sink: EventSink) -> Result<()> {
    let name = farm::node_name(farm_id, farm::Role::Control, None);
    let control = farm::connect(&name).await?;
    let result = async {
        farm::wait_until_booted(&control).await?;
        step(&sink, "Kolla multinode inventory에서 원본 compute 제거")?;
        let topology = Topology {
            name: name.clone(),
            ip: None,
            interface: farm::GUEST_INTERFACE.into(),
            computes: Vec::new(),
            multinode: true,
        };
        write_inventory(&control, &topology).await?;
        step(&sink, "원본 farm의 compute 등록 정리")?;
        stream(
            &control,
            &forget_computes_script(components),
            &format!("{LOG_DIR}/forget-computes.log"),
            &sink,
        )
        .await
    }
    .await;
    control.detach().await;
    result
}

/// Compute VMs are `farm-compute-<n>` to OpenStack; the control VM is never matched.
fn forget_computes_script(components: &[String]) -> String {
    let has = |id: &str| components.iter().any(|c| c == id);
    let known_hosts: String = (1..=farm::MAX_COMPUTES)
        .map(|index| {
            format!(
                "ssh-keygen -R {} >/dev/null 2>&1 || true
ssh-keygen -R {} >/dev/null 2>&1 || true
",
                farm::hostname(farm::Role::Compute, Some(index)),
                farm::address(farm::Role::Compute, Some(index)),
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
  case "$host" in farm-compute-*) ;; *) continue ;; esac
  echo "nova-compute $host: deleting service $id"
  $os compute service delete "$id"
done
# The OpenStack CLI has no placement commands without osc-placement; its SDK has them.
{OSC_VENV}/bin/python - <<'PY'
import openstack
conn = openstack.connect()
for rp in list(conn.placement.resource_providers()):
    if rp.name.startswith("farm-compute-"):
        print(f"placement: deleting resource provider {{rp.name}} {{rp.id}}", flush=True)
        conn.placement.delete_resource_provider(rp)
PY
docker exec nova_api nova-manage cell_v2 list_hosts | awk -F'|' 'NF >= 4 {{ gsub(/ /, "", $3); gsub(/ /, "", $4); if ($4 ~ /^farm-compute-/) print $3, $4 }}' | while read -r cell host; do
  echo "cell host mapping $host: deleting"
  docker exec nova_api nova-manage cell_v2 delete_host --cell_uuid "$cell" --host "$host"
done
"#
        ));
    }
    if has("neutron") {
        script.push_str(
            r#"$os network agent list -f value -c ID -c Host | while read -r id host; do
  case "$host" in farm-compute-*) ;; *) continue ;; esac
  echo "neutron agent on $host: deleting $id"
  $os network agent delete "$id"
done
"#,
        );
    }
    script.push_str("echo \"the source farm's computes are forgotten\"\n");
    script
}

/// Rewrite only Kolla's multinode inventory and Ansible concurrency, leaving passwords and
/// globals alone so an expansion cannot disturb the running control plane.
async fn write_inventory(deployer: &Sandbox, topology: &Topology) -> Result<()> {
    let template = deployer
        .fs()
        .read_to_string(&format!(
            "{VENV}/share/kolla-ansible/ansible/inventory/multinode"
        ))
        .await?;
    let inventory = config::multinode_inventory(
        &template,
        &farm::hostname(farm::Role::Control, None),
        &topology.computes,
    )?;
    let fs = deployer.fs();
    fs.write(INVENTORY, inventory).await?;
    run(deployer, "mkdir -p /etc/kolla/callback_plugins").await?;
    fs.write(
        "/etc/kolla/callback_plugins/farm.py",
        include_str!("farm_callback.py"),
    )
    .await?;
    let forks = 1 + topology.computes.len();
    fs.write(
        "/root/.ansible.cfg",
        &format!(
            "[defaults]\nhost_key_checking = False\nforks = {forks}\nstdout_callback = farm\ncallback_plugins = /etc/kolla/callback_plugins\n\n[ssh_connection]\npipelining = True\n"
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
marker=/etc/farm/template-wiped
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
if docker ps -a --format '{{{{.Names}}}}' | grep -qx openvswitch_db; then
  for _ in $(seq 60); do docker exec openvswitch_db ovs-vsctl show >/dev/null 2>&1 && break; sleep 2; done
  docker exec openvswitch_db ovs-vsctl set Open_vSwitch . external_ids:system-id={hostname}
fi"#
    )
}

/// Wait for OVS on the control VM to answer, then restart its OVS agent if it is not healthy.
/// Kolla's agent container exits when it starts while the database is still coming up, and
/// nothing restarts it afterwards; the farm would keep a network node without an agent.
const RESTART_OVS_AGENT: &str = r#"for _ in $(seq 40); do
  docker ps --format '{{.Names}} {{.Status}}' | grep -q '^openvswitch_db Up .* (healthy)' && break
  sleep 2
done
if ! docker ps --format '{{.Names}} {{.Status}}' | grep -q '^neutron_openvswitch_agent Up .* (healthy)'; then
  docker restart neutron_openvswitch_agent >/dev/null 2>&1 || true
fi"#;

/// The environment a bash `kolla-ansible` launcher needs. Up to 2024.1 it finds the playbooks
/// in `$VIRTUAL_ENV/share/kolla-ansible`, or, with no `VIRTUAL_ENV`, in what it takes for a
/// source checkout next to its own `bin/`; the farm's phase scripts never activate the venv,
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
state=''
for _ in $(seq 60); do
  state=$($os compute service list --service nova-compute --host {host} -f value -c State 2>/dev/null || true)
  [ "$state" = up ] && break
  sleep 5
done
[ "$state" = up ] || {{ echo "nova-compute {host} is not up: ${{state:-missing}}" >&2; exit 1; }}
echo "nova-compute {host} up"
"#
    );
    if neutron {
        // `--agent-type` takes a choice of short names that change between client versions
        // (`open-vswitch` now, the display name before); the host's agents are few, so their
        // own type column decides.
        script.push_str(&format!(
            r#"alive=''
for _ in $(seq 60); do
  alive=$($os network agent list --host {host} -f value -c "Agent Type" -c Alive 2>/dev/null \
    | awk '$1 == "Open" && $2 == "vSwitch" {{ print $NF; exit }}' || true)
  [ "$alive" = True ] && break
  sleep 5
done
[ "$alive" = True ] || {{ echo "Open vSwitch agent on {host} is not alive: ${{alive:-missing}}" >&2; exit 1; }}
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

/// Ready this host's compute VMs of a farm that spans hosts, as the prepare phase readies
/// computes, and let `ssh_public_key` in as root.
pub async fn prepare_share(
    farm_id: &str,
    components: &[String],
    release: &str,
    ssh_public_key: &str,
) -> Result<()> {
    let neutron = components.iter().any(|c| c == "neutron");
    let release = releases::info(release);
    let sink: EventSink = Arc::new(|_| Ok(()));
    for node in farm::get_farm(farm_id).await? {
        if node.role != Some(farm::Role::Compute) {
            continue;
        }
        let sandbox = farm::connect(&node.name).await?;
        let result = async {
            prepare_host(&sandbox, neutron, &release, &sink).await?;
            authorize_key(&sandbox, ssh_public_key).await
        }
        .await;
        sandbox.detach().await;
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
    farm_id: &str,
    from: Phase,
    deployer: &Sandbox,
    computes: &[(Compute, Option<Sandbox>)],
    topology: &Topology,
    exposure: Option<&Exposure>,
    components: &[String],
    release: &str,
    sink: &EventSink,
) -> Result<()> {
    for phase in Phase::ALL.into_iter().filter(|p| *p >= from) {
        sink(DeployEvent::Phase(phase))?;
        eprintln!("\n==> [{farm_id}] {phase}");
        let log = format!("{LOG_DIR}/{phase}.log");
        match phase {
            Phase::Prepare => {
                prepare(
                    deployer,
                    computes,
                    components,
                    &releases::info(release),
                    sink,
                )
                .await
            }
            Phase::Install => {
                let info = releases::info(release);
                stream(deployer, &install_script(&info), &log, sink).await
            }
            Phase::Configure => {
                configure(
                    deployer, computes, topology, exposure, components, release, sink,
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
                    &kolla_ansible("bootstrap-servers", &releases::info(release)),
                    &log,
                    sink,
                )
                .await
            }
            Phase::Prechecks => {
                step(sink, "kolla-ansible prechecks")?;
                stream(
                    deployer,
                    &kolla_ansible("prechecks", &releases::info(release)),
                    &log,
                    sink,
                )
                .await
            }
            Phase::Deploy => {
                step(sink, "kolla-ansible deploy")?;
                stream(
                    deployer,
                    &deploy_script(&releases::info(release)),
                    &log,
                    sink,
                )
                .await
            }
            Phase::PostDeploy => {
                step(sink, "kolla-ansible post-deploy (admin-openrc.sh 생성)")?;
                let script =
                    post_deploy_script(components, topology.multinode(), &releases::info(release));
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
                let script = verify_script(components, hosts);
                stream(deployer, &script, &log, sink).await
            }
        }
        .with_context(|| {
            format!("phase '{phase}' failed; resume with `farm deploy {farm_id} --from {phase}`")
        })?;
        sink(DeployEvent::Output(format!("phase {phase} completed\n")))?;
    }
    let url = if let Some(exposure) = exposure {
        exposure.urls(Some(components))["keystone"].clone()
    } else {
        format!("http://{}:5000/v3", topology.vip()?)
    };
    eprintln!(
        "\n==> [{farm_id}] ready: {url} (credentials: {CONFIG_DIR}/admin-openrc.sh on {})",
        topology.name
    );
    Ok(())
}

/// Ready every VM for Kolla. The deployer (all-in-one or control VM) also hosts Neutron's
/// external network; a multi-node farm's computes only run OVS, Nova and Docker.
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
                "/etc/farm/external.conf",
                format!(
                    "EXTERNAL_NETWORK={EXTERNAL_NETWORK}\nEXTERNAL_GATEWAY={EXTERNAL_GATEWAY}/24\n"
                ),
            )
            .await?;
        deployer
            .fs()
            .write(
                "/etc/farm/neutron-sitecustomize.py",
                include_str!("neutron_sitecustomize.py"),
            )
            .await?;
        // Golden images now install nftables; farms made from the former Docker-built
        // image may predate it.
        run(deployer, "command -v nft >/dev/null || dnf install -y -q nftables; systemctl restart farm-external.service").await?;
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
    farm::wait_until_booted(sandbox).await?;
    step(sink, "farm-identity·farm-hostname 유닛 적용")?;
    // Farms made by an older build get the current units too. msb resets /etc/hosts on
    // reboot, so restore the hostname mapping before Docker creates host-network containers.
    farm::install_guest_files(sandbox).await?;
    run(
        sandbox,
        "systemctl restart farm-identity.service farm-hostname.service",
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
    // start, which also waits for a multi-node farm's links.
    run(
        sandbox,
        r#"set -euo pipefail
mkdir -p /etc/udev/rules.d /etc/systemd/system/docker.service.d
cat > /etc/systemd/system/docker.service.d/farm-mounts.conf <<'EOF'
[Unit]
Requires=farm-hostname.service
After=farm-link@farm0.service farm-link@farm1.service farm-identity.service farm-hostname.service farm-external.service

[Service]
ExecStartPre=/usr/bin/mount --make-shared /run
ExecStartPre=/usr/bin/sh -c 'if [ -c /dev/kvm ]; then chown root:42427 /dev/kvm && chmod 0666 /dev/kvm; fi'
EOF
mount --make-shared /run
if [ -c /dev/kvm ]; then chown root:42427 /dev/kvm && chmod 0666 /dev/kvm; fi
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
const UV_VENV: &str = "/opt/farm-uv";
/// Where uv installs older minors next to the guest's own python3.
const PYTHON_DIR: &str = "/opt/farm-python";

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
  echo "this VM has no python3; install it in the golden image and start the farm again" >&2
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
  echo "check that the VM reaches pypi.org (uv itself) and github.com (the interpreter), then start the farm again" >&2
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
if ! ldconfig -p | grep -q 'libcrypt\.so\.1'; then
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
const LEGACY_BRIDGE_TASK: &str = r#"- name: Create the farm's bridges with the userspace datapath
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
/// on the network node it breaks the router and DHCP ports and the farm link.
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
            to: "# farm: dnf5 dropped delta RPMs, and Rocky 10 has no drpm package.\n".into(),
        });
    }
    patches.push(LegacyPatch {
        // A control VM alone has no compute host yet; the farm's globals say so, and the first
        // compute joins later without redeploying the control plane.
        aim: "no-compute precheck",
        file: "roles/nova-cell/tasks/precheck.yml",
        from: "  when:\n    - groups[nova_cell_compute_group] | length < 1\n    - not nova_compute_ironic.enabled | bool\n",
        to: "  when:\n    - groups[nova_cell_compute_group] | length < 1\n    - not nova_compute_ironic.enabled | bool\n    - not (farm_no_computes | default(false))\n".into(),
    });
    patches.push(LegacyPatch {
        // The same kernel has no openvswitch module; farms run OVS with the userspace datapath.
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
            "unexpected Kolla task for the %s patch in %s (%d matches); review the farm patch for this release"
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
# Only on compute-only VMs: on the network node it breaks the router and DHCP ports and the farm link,
# so all-in-one farms still drop instance TCP.
text = path.read_text()
marker = "    - { col: \"other_config\", name: \"hw-offload\", value: true, state: \"{{ 'present' if openvswitch_hw_offload | bool else 'absent' }}\" }\n"
patched = marker + "    - { col: \"other_config\", name: \"userspace-tso-enable\", value: true, state: \"{{ 'present' if ovs_datapath == 'netdev' and inventory_hostname in groups['compute'] and inventory_hostname not in groups['network'] else 'absent' }}\" }\n"
if patched not in text:
    if text.count(marker) != 1:
        raise RuntimeError("unexpected Kolla OVS other_config task; review the userspace TSO patch")
    path.write_text(text.replace(marker, patched, 1))
# A control VM alone has no compute host yet; the farm's globals say so, and the first
# compute joins later without redeploying the control plane.
path = Path("{VENV}/share/kolla-ansible/ansible/roles/nova-cell/tasks/precheck.yml")
text = path.read_text()
marker = "  when:\n    - groups[nova_cell_compute_group] | length < 1\n    - not nova_compute_ironic.enabled | bool\n"
patched = marker + "    - not (farm_no_computes | default(false))\n"
if patched not in text:
    if text.count(marker) != 1:
        raise RuntimeError("unexpected Kolla nova-cell precheck; review the no-compute patch")
    path.write_text(text.replace(marker, patched, 1))
# The same kernel has no openvswitch module; farms run OVS with the userspace (netdev) datapath.
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

fn install_script(release: &releases::ReleaseInfo) -> String {
    format!(
        r#"set -euo pipefail
# Golden images install the build tools; farms made from the former Docker-built image lack them.
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
if [ ! -x {VENV}/bin/kolla-ansible ] || [ "$(cat {VENV}/.farm-release 2>/dev/null || true)" != "$stamp" ]; then
  step "kolla-ansible 가상환경 생성 ({VENV}, $python)"
  rm -rf {VENV}
  "$python" -m venv {VENV}
  {VENV}/bin/pip install -q -U pip
  step "kolla-ansible 설치 ({reference})"
  {VENV}/bin/pip install -q "git+https://opendev.org/openstack/kolla-ansible@{reference}"
{ansible_install}  printf '%s' "$stamp" > {VENV}/.farm-release
fi
{six_fix}{ansible_fixes}export PATH={VENV}/bin:$PATH{venv_export}
{galaxy}{install_deps}
{kernel_patches}
if [ ! -x {OSC_VENV}/bin/openstack ] || [ "$(cat {OSC_VENV}/.farm-release 2>/dev/null || true)" != "$stamp" ]; then
  step "OpenStack CLI 가상환경 생성 ({OSC_VENV})"
  rm -rf {OSC_VENV}
  "$python" -m venv {OSC_VENV}
  {OSC_VENV}/bin/pip install -q -U pip
  step "python-openstackclient 설치"
  {OSC_VENV}/bin/pip install -q -c https://releases.openstack.org/constraints/upper/{series} python-openstackclient
  printf '%s' "$stamp" > {OSC_VENV}/.farm-release
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

async fn configure(
    deployer: &Sandbox,
    computes: &[(Compute, Option<Sandbox>)],
    topology: &Topology,
    exposure: Option<&Exposure>,
    components: &[String],
    release: &str,
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

    // A control VM alone is already a multi-node farm: its computes join later, so its
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
                    compute.hostname
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
        config::multinode_inventory(
            &template,
            &farm::hostname(farm::Role::Control, None),
            &peers,
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
            let (Some(port), Some(variable)) = (&c.public, public_url_variable(c.id)) else {
                continue;
            };
            if components.iter().any(|id| id == c.id) {
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
        globals.push_str("farm_no_computes: true\n");
    }
    fs.write(&format!("{CONFIG_DIR}/globals.yml"), globals)
        .await?;
    // Horizon sends users to the Compute overview after login, which answers 403 without Nova.
    let horizon_settings = format!("{CONFIG_DIR}/config/horizon/_9999-custom-settings.py");
    if components.iter().any(|c| c == "horizon") && !components.iter().any(|c| c == "nova") {
        run(deployer, &format!("mkdir -p {CONFIG_DIR}/config/horizon")).await?;
        fs.write(
            &horizon_settings,
            "# Written by farm: this farm has no Nova, so start on the image list.\nHORIZON_CONFIG[\"user_home\"] = \"/project/images/\"\n",
        )
        .await?;
    } else {
        run(deployer, &format!("rm -f {horizon_settings}")).await?;
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
        "/etc/kolla/callback_plugins/farm.py",
        include_str!("farm_callback.py"),
    )
    .await?;
    fs.write(
        "/root/.ansible.cfg",
        &format!(
            "[defaults]\nhost_key_checking = False\nforks = {forks}\nstdout_callback = farm\ncallback_plugins = /etc/kolla/callback_plugins\n\n[ssh_connection]\npipelining = True\n"
        ),
    )
    .await?;
    eprintln!(
        "wrote {CONFIG_DIR}/globals.yml and {INVENTORY} (vip {})",
        topology.vip()?
    );
    Ok(())
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
                compute.hostname
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
grep -qxF '{key}' "$f" || {{ [ -s "$f" ] && [ -n "$(tail -c1 "$f")" ] && echo >> "$f"; echo '{key}' >> "$f"; }}"#
        ),
    )
    .await?;
    Ok(())
}

/// Wait until a deployed farm's services answer again after its VM started from disk.
/// With `rebind`, first point the public catalog entries recorded for `from` at `to`:
/// a farm restored from a snapshot publishes the services on its own host ports.
pub async fn settle(
    farm_id: &str,
    rebind: Option<(&Exposure, &Exposure)>,
    components: &[String],
    sink: EventSink,
) -> Result<()> {
    let name = farm::all_in_one_name(farm_id).await?;
    let sandbox = farm::connect(&name).await?;
    let result = async {
        farm::wait_until_booted(&sandbox).await?;
        stream(
            &sandbox,
            &settle_script(rebind, components),
            &format!("{LOG_DIR}/settle.log"),
            &sink,
        )
        .await
    }
    .await;
    sandbox.detach().await;
    result
}

/// Public base URL prefixes of `from`, keyed to the same services on `to`.
fn rebind_map(from: &Exposure, to: &Exposure) -> BTreeMap<String, String> {
    crate::model::COMPONENTS
        .iter()
        .filter_map(|c| c.public.as_ref())
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
  if [ "$(echo "$agents" | grep -c True)" -ge 4 ] && ! echo "$agents" | grep -q False; then break; fi
  sleep 3
done
echo "$agents"
for agent in "Open vSwitch agent" "DHCP agent" "L3 agent" "Metadata agent"; do
  echo "$agents" | grep -q "^$agent True" || { echo "$agent is not alive" >&2; exit 1; }
done
echo "neutron OK"
"#,
        );
    }
    if has("nova") {
        script.push_str(&format!(
            r#"compute_state=''
# A farm restored from a control template has no compute VM, so no nova-compute, until one
# is added; Kolla's multinode inventory then lists no compute host.
if {no_computes}; then
  $os compute service list -f value -c ID >/dev/null
  echo "nova OK (no compute VM yet)"
else
# Every nova-compute, one per compute VM of a multi-node farm.
for _ in $(seq 60); do
  compute_state=$($os compute service list --service nova-compute -f value -c State 2>/dev/null || true)
  [ -n "$compute_state" ] && ! echo "$compute_state" | grep -qvx up && break
  sleep 5
done
[ -n "$compute_state" ] && ! echo "$compute_state" | grep -qvx up || {{ echo "nova-compute is not up everywhere: ${{compute_state:-missing}}" >&2; exit 1; }}
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
        r#"[ -f {inventory} ] && grep -qx '\[compute\]' {inventory} && [ "$(awk '/^\[/ {{ group = $0; next }} group == "[compute]" && NF && !/^#/ {{ n++ }} END {{ print n + 0 }}' {inventory})" = 0 ]"#
    )
}

/// Kolla variable holding each service's public base URL (without the API version path).
fn public_url_variable(component: &str) -> Option<&'static str> {
    Some(match component {
        "keystone" => "keystone_public_url",
        "glance" => "glance_public_endpoint",
        "placement" => "placement_public_endpoint",
        "horizon" => "horizon_public_endpoint",
        "neutron" => "neutron_public_endpoint",
        "nova" => "nova_public_base_endpoint",
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
        // A shared external network for routers and floating IPs, created once per farm.
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
fn verify_script(components: &[String], computes: usize) -> String {
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
image_id=$($os image create --disk-format raw --container-format bare --file "$work/upload.img" "farm-verify-$(cat /proc/sys/kernel/random/uuid)" -f value -c id)
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
region=$(sed -n 's/.*name="region" value="\([^"]*\)".*/\1/p' "$work/login.html" | head -1)
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
    }
    script
}

/// Builds a throwaway tenant network behind a router on `public`, then proves the
/// userspace datapath forwards: the all-in-one host pings the router's gateway and,
/// from the router namespace, the DHCP port on the tenant network.
fn neutron_verify_script() -> String {
    r#"step "Neutron agent 상태 확인 (OVS·DHCP·L3·Metadata)"
for _ in $(seq 60); do
  agents=$($os network agent list -f value -c "Agent Type" -c Alive)
  if [ "$(echo "$agents" | grep -c True)" -ge 4 ] && ! echo "$agents" | grep -q False; then break; fi
  sleep 3
done
echo "$agents"
for agent in "Open vSwitch agent" "DHCP agent" "L3 agent" "Metadata agent"; do
  echo "$agents" | grep -q "^$agent True" || { echo "$agent is not alive" >&2; exit 1; }
done
step "테넌트 네트워크·라우터·DHCP 포트 검증"
name="farm-verify-$(cut -c1-8 /proc/sys/kernel/random/uuid)"
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
arch=$(uname -m)
case "$arch" in
  aarch64|arm64)
    image_url=https://download.cirros-cloud.net/0.6.3/cirros-0.6.3-aarch64-disk.img
    image_props='--property hw_firmware_type=uefi --property hw_machine_type=virt'
    ;;
  *)
    image_url=https://download.cirros-cloud.net/0.6.3/cirros-0.6.3-x86_64-disk.img
    image_props=''
    ;;
esac
image_name=cirros-farm
if ! $os image show "$image_name" >/dev/null 2>&1; then
  curl -fsSL -o "$work/cirros.img" "$image_url"
  # shellcheck disable=SC2086
  $os image create --disk-format qcow2 --container-format bare --property os_distro=cirros $image_props --file "$work/cirros.img" "$image_name" >/dev/null
fi
name="farm-vm-$(cut -c1-8 /proc/sys/kernel/random/uuid)"
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

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
            legacy.contains("UV_PYTHON_INSTALL_DIR=/opt/farm-python"),
            "{legacy}"
        );
        assert!(
            legacy.contains("/opt/farm-uv/bin/uv python install 3.8"),
            "{legacy}"
        );
        assert!(
            legacy.contains(r#"python="$(/opt/farm-uv/bin/uv python find 3.8"#),
            "{legacy}"
        );
        // uv comes from a venv of its own: Rocky refuses pip into the system interpreter.
        assert!(legacy.contains("python3 -m venv /opt/farm-uv"), "{legacy}");
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
        assert!(!current.contains("farm-python"), "{current}");
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
        // fixtures pin what the farm generates for one now, so a change there shows up here.
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
        assert!(read("roles/nova-cell/tasks/precheck.yml").contains("farm_no_computes"));
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

            // The bridge is created with the farm's datapath type before Kolla's own task adds
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
                precheck.contains("- not (farm_no_computes | default(false))"),
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
                    baremetal.contains("# farm: dnf5 dropped delta RPMs"),
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
            verify_script(&components, 1),
            verify_script(&components, 2),
            verify_script(&components, 0),
            settle_script(None, &components),
            settle_script(Some((&from, &to)), &components),
            forget_computes_script(&components),
            forget_computes_script(&components[..2]),
            OVS_EXTEND_START_SCRIPT.to_owned(),
            include_str!("../../guest/farm/farm-external").to_owned(),
            include_str!("../../guest/farm/farm-identity").to_owned(),
            // Deploying a farm and adding a compute read the guest's address the same way.
            identity_command(),
            // A compute booted from a template recreates the containers it inherited.
            wipe_template_script("farm-compute-2"),
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
            verify_script(&components, 1),
            verify_script(&components, 2),
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
            "farm patch for this release",
        ] {
            assert!(!current.contains(absent), "{absent}: {current}");
        }

        // A restarted OVS container can leave the control's agent dead; the expansion revives it.
        assert!(RESTART_OVS_AGENT.contains("docker restart neutron_openvswitch_agent"));

        // The agent check reads the host's own agents: `--agent-type` takes names that differ
        // between client versions, and a rejected filter looked like a dead agent.
        let verify = verify_compute_script("farm-compute-2", true);
        assert!(verify.contains(r#"-c "Agent Type" -c Alive"#));
        assert!(!verify.contains("--agent-type"));

        // A template's containers carry its hostname; the copy drops the ones that are not its
        // own, so Kolla's OVS agent registers under this VM and not the source one.
        let wipe = wipe_template_script("farm-compute-2");
        assert!(wipe.contains(r#"if [ -n "$h" ] && [ "$h" != "$me" ]"#));
        assert!(wipe.contains(r#"docker rm -f "$c""#));
        // The host's own OVS containers carry the farm's network state, not a copy's identity.
        assert!(wipe.contains("openvswitch_db | openvswitch_vswitchd) continue"));

        // A control VM alone deploys as a multi-node farm: its external network is physnet2
        // on br-ex, and the computes that join later keep the same layout.
        let control_only = Topology::from_nodes(&[farm::FarmNode {
            name: "dev-control".into(),
            role: Some(farm::Role::Control),
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
        let alone = verify_script(&components, 0);
        assert!(alone.contains("no compute VM yet") && !alone.contains("nova-compute is not up"));

        let verify = verify_script(&components, 1);
        assert!(
            !verify.contains("ssh "),
            "all-in-one verification must not require distributed SSH keys"
        );
        assert!(verify.contains("ip netns exec \"qrouter-$router_id\""));
        assert!(verify.contains("nova-compute is not up"));
        assert!(verify.contains("cirros-0.6.3-aarch64-disk.img"));
        for script in [
            install_script(&releases::info(OPENSTACK_RELEASE)),
            verify_script(&components, 1),
            verify_script(&components, 2),
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
            "ssh-keygen -R farm-compute-1 ",
            "ssh-keygen -R 10.254.0.13 ",
            "$os compute service delete \"$id\"",
            "conn.placement.delete_resource_provider(rp)",
            "nova-manage cell_v2 delete_host --cell_uuid \"$cell\" --host \"$host\"",
            "$os network agent delete \"$id\"",
        ] {
            assert!(script.contains(expected), "{expected}");
        }
        // Nothing of the control VM's own matches.
        assert!(!script.contains("farm-control"));
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
            hostname: "farm-compute-1".into(),
            address: "10.254.0.11".into(),
        };
        assert!(check(
            &config::multinode_inventory(template, "farm-control", &[]).unwrap()
        ));
        assert!(!check(
            &config::multinode_inventory(template, "farm-control", &[compute]).unwrap()
        ));
        assert!(!check("[control]\nlocalhost\n\n[compute]\nlocalhost\n"));
        assert!(settle_script(None, &components).contains("no compute VM yet"));
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
            &exposure("farm.example", 15010),
        );
        assert_eq!(map["http://localhost:15000"], "http://farm.example:15010");
        assert_eq!(map["http://localhost:15005"], "http://farm.example:15015");
        assert_eq!(map.len(), crate::model::COMPONENTS.len());
        // Run the generated rewrite against a stub client and globals.yml.
        let dir = tempfile::tempdir().unwrap();
        let script = settle_script(
            Some((&exposure("localhost", 15000), &exposure("localhost", 15010))),
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
        assert!(calls.contains("endpoint set --url http://localhost:15010 k"));
        assert!(calls.contains("endpoint set --url http://localhost:15015/v2.1 n"));
        assert!(!calls.contains(" o\n"), "{calls}");
        let globals = std::fs::read_to_string(dir.path().join("globals.yml")).unwrap();
        assert!(globals.contains("keystone_public_url: \"http://localhost:15010\""));
        assert!(globals.contains("glance_public_endpoint: \"http://localhost:15011\""));
        assert!(globals.contains("10.254.0.2"));
    }
}
