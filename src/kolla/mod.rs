mod config;

use std::fmt;

use anyhow::{Context, Result};
use microsandbox::Sandbox;

use crate::farm;
use crate::model::Exposure;
use crate::shell::{run, stream};
pub use config::OPENSTACK_RELEASE;
use config::Topology;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug)]
pub enum DeployEvent {
    Phase(Phase),
    Output(String),
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
    deploy_with(farm_id, from, None, components, Arc::new(|_| Ok(()))).await
}

/// `components` is the farm plan's resolved service list.
pub async fn deploy_with(
    farm_id: &str,
    from: Phase,
    exposure: Option<Exposure>,
    components: &[String],
    sink: EventSink,
) -> Result<()> {
    let mut topology = Topology::from_nodes(&farm::get_farm(farm_id).await?)?;
    let deployer = farm::connect(&topology.name).await?;
    let result = async {
        farm::wait_until_booted(&deployer).await?;
        // Farms created with a fixed guest identity bind every service to it; older farms use eth0.
        let identity = run(
            &deployer,
            &format!(
                "if [ -f {conf} ]; then . {conf}; echo \"{dummy} $FARM_ADDRESS\"; else echo \"eth0 $(ip -4 -o addr show dev eth0 | awk '{{split($4,a,\"/\"); print a[1]; exit}}')\"; fi",
                conf = farm::IDENTITY_CONF,
                dummy = farm::GUEST_INTERFACE,
            ),
        )
        .await?;
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
            &topology,
            exposure.as_ref(),
            components,
            &sink,
        )
        .await
    }
    .await;
    deployer.detach().await;
    result
}

async fn deploy_phases(
    farm_id: &str,
    from: Phase,
    deployer: &Sandbox,
    topology: &Topology,
    exposure: Option<&Exposure>,
    components: &[String],
    sink: &EventSink,
) -> Result<()> {
    for phase in Phase::ALL.into_iter().filter(|p| *p >= from) {
        sink(DeployEvent::Phase(phase))?;
        eprintln!("\n==> [{farm_id}] {phase}");
        let log = format!("{LOG_DIR}/{phase}.log");
        match phase {
            Phase::Prepare => prepare(deployer, components).await,
            Phase::Install => stream(deployer, &install_script(), &log, sink).await,
            Phase::Configure => configure(deployer, topology, exposure, components).await,
            Phase::Bootstrap => {
                stream(deployer, &kolla_ansible("bootstrap-servers"), &log, sink).await
            }
            Phase::Prechecks => stream(deployer, &kolla_ansible("prechecks"), &log, sink).await,
            Phase::Deploy => stream(deployer, &kolla_ansible("deploy"), &log, sink).await,
            Phase::PostDeploy => {
                stream(deployer, &post_deploy_script(components), &log, sink).await
            }
            Phase::Verify => stream(deployer, &verify_script(components), &log, sink).await,
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

async fn prepare(sandbox: &Sandbox, components: &[String]) -> Result<()> {
    farm::wait_until_booted(sandbox).await?;
    // Upgrade cached golden images too. msb resets /etc/hosts on reboot, so
    // restore the live eth0 hostname before Docker creates host-network containers.
    sandbox
        .fs()
        .write(
            "/usr/local/sbin/farm-identity",
            include_str!("../../docker/farm-identity"),
        )
        .await?;
    sandbox
        .fs()
        .write(
            "/etc/systemd/system/farm-identity.service",
            include_str!("../../docker/farm-identity.service"),
        )
        .await?;
    sandbox
        .fs()
        .write(
            "/usr/local/sbin/farm-hostname",
            include_str!("../../docker/farm-hostname"),
        )
        .await?;
    sandbox
        .fs()
        .write(
            "/etc/systemd/system/farm-hostname.service",
            include_str!("../../docker/farm-hostname.service"),
        )
        .await?;
    sandbox
        .fs()
        .write(
            "/usr/local/sbin/farm-external",
            include_str!("../../docker/farm-external"),
        )
        .await?;
    sandbox
        .fs()
        .write(
            "/etc/systemd/system/farm-external.service",
            include_str!("../../docker/farm-external.service"),
        )
        .await?;
    run(sandbox, "chmod 755 /usr/local/sbin/farm-identity /usr/local/sbin/farm-hostname /usr/local/sbin/farm-external; systemctl daemon-reload; systemctl enable farm-identity.service farm-hostname.service farm-external.service; systemctl restart farm-identity.service farm-hostname.service").await?;
    // Kolla toolbox bind-mounts /run with shared propagation, and nova_libvirt runs QEMU
    // as uid/gid 42427 without systemd-udevd in the guest. Apply both now and on every Docker start.
    run(
        sandbox,
        r#"set -euo pipefail
mkdir -p /etc/udev/rules.d /etc/systemd/system/docker.service.d
cat > /etc/systemd/system/docker.service.d/farm-mounts.conf <<'EOF'
[Unit]
Requires=farm-hostname.service
After=farm-identity.service farm-hostname.service farm-external.service

[Service]
ExecStartPre=/usr/bin/mount --make-shared /run
ExecStartPre=/usr/bin/sh -c 'if [ -c /dev/kvm ]; then chown root:42427 /dev/kvm && chmod 0666 /dev/kvm; fi'
EOF
mount --make-shared /run
if [ -c /dev/kvm ]; then chown root:42427 /dev/kvm && chmod 0666 /dev/kvm; fi
systemctl daemon-reload"#,
    )
    .await?;
    if components.iter().any(|c| c == "neutron") {
        sandbox
            .fs()
            .write(
                "/etc/farm/external.conf",
                format!(
                    "EXTERNAL_NETWORK={EXTERNAL_NETWORK}\nEXTERNAL_GATEWAY={EXTERNAL_GATEWAY}/24\n"
                ),
            )
            .await?;
        sandbox
            .fs()
            .write(OVS_EXTEND_START, OVS_EXTEND_START_SCRIPT)
            .await?;
        sandbox
            .fs()
            .write(
                "/etc/farm/neutron-sitecustomize.py",
                include_str!("neutron_sitecustomize.py"),
            )
            .await?;
        run(sandbox, "rpm -q nftables >/dev/null || dnf install -y -q nftables; systemctl restart farm-external.service").await?;
    }
    Ok(())
}

fn install_script() -> String {
    format!(
        r#"set -euo pipefail
export PATH={VENV}/bin:$PATH
dnf install -y -q gcc python3-devel libffi-devel openssl-devel
if [ ! -x {VENV}/bin/kolla-ansible ]; then
  python3 -m venv {VENV}
  {VENV}/bin/pip install -q -U pip
  {VENV}/bin/pip install -q "git+https://opendev.org/openstack/kolla-ansible@stable/{OPENSTACK_RELEASE}"
fi
kolla-ansible install-deps
# Retain the guest-kernel compatibility patches for Neutron's userspace OVS.
python3 - <<'PY'
from pathlib import Path
# Kolla creates br-ex without a datapath type, so with the netdev datapath it stays unusable
# ("No datapath_id on bridge br-ex"). `set` only applies when the bridge is first created.
path = Path("{VENV}/share/kolla-ansible/ansible/roles/openvswitch/tasks/post-config.yml")
text = path.read_text()
marker = "      fail_mode: standalone\n  loop: \"{{{{ neutron_bridge_name.split(',') }}}}\"\n"
patched = "      fail_mode: standalone\n      set: \"Bridge {{{{ item }}}} datapath_type={{{{ ovs_datapath }}}}\"\n  loop: \"{{{{ neutron_bridge_name.split(',') }}}}\"\n"
if patched not in text:
    if text.count(marker) != 1:
        raise RuntimeError("unexpected Kolla OVS bridge task; review the datapath_type patch")
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
PY
if [ ! -x {OSC_VENV}/bin/openstack ]; then
  python3 -m venv {OSC_VENV}
  {OSC_VENV}/bin/pip install -q -U pip
  {OSC_VENV}/bin/pip install -q -c https://releases.openstack.org/constraints/upper/{OPENSTACK_RELEASE} python-openstackclient
fi
kolla-ansible --version"#
    )
}

async fn configure(
    deployer: &Sandbox,
    topology: &Topology,
    exposure: Option<&Exposure>,
    components: &[String],
) -> Result<()> {
    run(
        deployer,
        &format!(
            "mkdir -p {CONFIG_DIR}/config && \
             cp -n {VENV}/share/kolla-ansible/etc_examples/kolla/passwords.yml {CONFIG_DIR}/passwords.yml && \
             {VENV}/bin/kolla-genpwd -p {CONFIG_DIR}/passwords.yml"
        ),
    )
    .await?;

    let arch = run(deployer, "uname -m").await?;
    let kvm = run(
        deployer,
        "if [ -c /dev/kvm ]; then echo true; else echo false; fi",
    )
    .await?
    .trim()
        == "true";
    let template = deployer
        .fs()
        .read_to_string(&format!(
            "{VENV}/share/kolla-ansible/ansible/inventory/all-in-one"
        ))
        .await?;
    // Everything runs on one VM, including the Ansible controller.
    let forks = 1;
    let fs = deployer.fs();
    let mut globals = config::globals(
        topology.vip()?,
        &topology.interface,
        arch.trim(),
        kvm,
        components,
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
    if components.iter().any(|c| c == "neutron") {
        run(deployer, &format!("mkdir -p {CONFIG_DIR}/config/neutron")).await?;
        fs.write(
            &ovs_agent,
            "[securitygroup]\nfirewall_driver = openvswitch\n",
        )
        .await?;
    } else {
        run(deployer, &format!("rm -f {ovs_agent}")).await?;
    }
    fs.write(
        &format!("{CONFIG_DIR}/config/galera.cnf"),
        config::GALERA_CONFIG,
    )
    .await?;
    fs.write(INVENTORY, template).await?;
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

/// Wait until a deployed farm's services answer again after its VM started from disk.
/// With `rebind`, first point the public catalog entries recorded for `from` at `to`:
/// a farm restored from a snapshot publishes the services on its own host ports.
pub async fn settle(
    farm_id: &str,
    rebind: Option<(&Exposure, &Exposure)>,
    components: &[String],
    sink: EventSink,
) -> Result<()> {
    let name = farm::node_name(farm_id, farm::Role::AllInOne);
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
        script.push_str(
            r#"compute_state=''
for _ in $(seq 60); do
  compute_state=$($os compute service list --service nova-compute -f value -c State 2>/dev/null | head -1)
  [ "$compute_state" = up ] && break
  sleep 5
done
[ "$compute_state" = up ] || { echo "nova-compute is not up: ${compute_state:-missing}" >&2; exit 1; }
echo "nova OK"
"#,
        );
    }
    script.push_str("echo \"services answer\"\n");
    script
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

fn post_deploy_script(components: &[String]) -> String {
    let mut script = kolla_ansible("post-deploy");
    if components.iter().any(|c| c == "neutron") {
        // A shared external network for routers and floating IPs, created once per farm.
        script.push_str(&format!(
            r#"
set -euo pipefail
. {CONFIG_DIR}/admin-openrc.sh
os={OSC_VENV}/bin/openstack
if ! $os network show public >/dev/null 2>&1; then
  $os network create --external --share --provider-network-type flat --provider-physical-network physnet1 public
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

fn kolla_ansible(command: &str) -> String {
    // kolla-ansible shells out to ansible-playbook and ansible-galaxy from PATH.
    format!("PATH={VENV}/bin:$PATH kolla-ansible {command} -i {INVENTORY}")
}

fn verify_script(components: &[String]) -> String {
    let has = |id: &str| components.iter().any(|c| c == id);
    let mut script = format!(
        r#"set -euo pipefail
. {CONFIG_DIR}/admin-openrc.sh
os={OSC_VENV}/bin/openstack
$os token issue -f value -c expires
$os endpoint list --service glance -f value -c interface -c url
work=$(mktemp -d)
image_id=''
extra_cleanup() {{ :; }}
cleanup() {{
  if [ -n "$image_id" ]; then $os image delete "$image_id"; fi
  extra_cleanup || true
  rm -rf "$work"
}}
trap cleanup EXIT
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
            r#"token=$($os token issue -f value -c id)
placement=$($os endpoint list --service placement --interface internal -f value -c url)
curl -sSf -o /dev/null -H "X-Auth-Token: $token" -H "OpenStack-API-Version: placement 1.0" "$placement/resource_providers"
echo "placement OK"
"#,
        );
    }
    if has("horizon") {
        // Log in directly like a browser: exercise Django, Keystone and memcached sessions.
        script.push_str(
            r#"horizon="http://$(awk -F'"' '/^kolla_internal_vip_address:/ {print $2}' /etc/kolla/globals.yml)"
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
        script.push_str(&nova_verify_script());
    }
    script
}

/// Builds a throwaway tenant network behind a router on `public`, then proves the
/// userspace datapath forwards: the all-in-one host pings the router's gateway and,
/// from the router namespace, the DHCP port on the tenant network.
fn neutron_verify_script() -> String {
    r#"for _ in $(seq 60); do
  agents=$($os network agent list -f value -c "Agent Type" -c Alive)
  if [ "$(echo "$agents" | grep -c True)" -ge 4 ] && ! echo "$agents" | grep -q False; then break; fi
  sleep 3
done
echo "$agents"
for agent in "Open vSwitch agent" "DHCP agent" "L3 agent" "Metadata agent"; do
  echo "$agents" | grep -q "^$agent True" || { echo "$agent is not alive" >&2; exit 1; }
done
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

/// Boots a tiny Cirros instance and pings it from the router namespace.
fn nova_verify_script() -> String {
    r#"for _ in $(seq 40); do
  compute_state=$($os compute service list --service nova-compute -f value -c State | head -1)
  [ "$compute_state" = up ] && break
  sleep 5
done
[ "$compute_state" = up ] || { echo "nova-compute is not up: ${compute_state:-missing}" >&2; exit 1; }
$os flavor show m1.tiny >/dev/null 2>&1 || $os flavor create --ram 512 --disk 1 --vcpus 1 m1.tiny >/dev/null
$os security group rule create --ingress --protocol icmp --ethertype IPv4 default >/dev/null 2>&1 || true
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
extra_cleanup
server_id=''; router_id=''; net_id=''
echo "nova OK: $name at $fixed answered from the router"
"#
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

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
        let scripts = [
            install_script(),
            post_deploy_script(&components),
            verify_script(&components),
            settle_script(None, &components),
            settle_script(Some((&from, &to)), &components),
            OVS_EXTEND_START_SCRIPT.to_owned(),
            include_str!("../../docker/farm-external").to_owned(),
            include_str!("../../docker/farm-identity").to_owned(),
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
        let verify = verify_script(&components);
        assert!(
            !verify.contains("ssh "),
            "all-in-one verification must not require distributed SSH keys"
        );
        assert!(verify.contains("ip netns exec \"qrouter-$router_id\""));
        assert!(verify.contains("nova-compute is not up"));
        assert!(verify.contains("cirros-0.6.3-aarch64-disk.img"));
        assert!(!settle_script(None, &components).contains("endpoint set"));
        assert!(!settle_script(Some((&from, &from)), &components).contains("endpoint set"));
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
