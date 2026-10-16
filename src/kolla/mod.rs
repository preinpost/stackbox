mod config;

use std::fmt;

use anyhow::{Context, Result};
use futures::future::try_join_all;
use microsandbox::Sandbox;

use crate::farm;
use crate::shell::{run, stream};
use config::{OPENSTACK_RELEASE, Topology};

const VENV: &str = "/opt/kolla";
const OSC_VENV: &str = "/opt/osc";
const CONFIG_DIR: &str = "/etc/kolla";
const INVENTORY: &str = "/etc/kolla/inventory";
const LOG_DIR: &str = "/var/log/farm";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum Phase {
    /// Wait for systemd and hand the deployer's SSH key to every target
    Prepare,
    /// Install kolla-ansible and the OpenStack client on the deployer
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

pub async fn deploy(farm_id: &str, from: Phase) -> Result<()> {
    let topology = Topology::from_nodes(&farm::get_farm(farm_id).await?)?;
    let deployer = farm::connect(&topology.deployer).await?;
    for phase in Phase::ALL.into_iter().filter(|p| *p >= from) {
        println!("\n==> [{farm_id}] {phase}");
        let log = format!("{LOG_DIR}/{phase}.log");
        match phase {
            Phase::Prepare => prepare(&deployer, &topology).await,
            Phase::Install => stream(&deployer, &install_script(), &log).await,
            Phase::Configure => configure(&deployer, &topology).await,
            Phase::Bootstrap => stream(&deployer, &kolla_ansible("bootstrap-servers"), &log).await,
            Phase::Prechecks => stream(&deployer, &kolla_ansible("prechecks"), &log).await,
            Phase::Deploy => stream(&deployer, &kolla_ansible("deploy"), &log).await,
            Phase::PostDeploy => stream(&deployer, &kolla_ansible("post-deploy"), &log).await,
            Phase::Verify => stream(&deployer, &verify_script(), &log).await,
        }
        .with_context(|| {
            format!("phase '{phase}' failed; resume with `farm deploy {farm_id} --from {phase}`")
        })?;
    }
    println!(
        "\n==> [{farm_id}] ready: http://{}:5000/v3 (credentials: {CONFIG_DIR}/admin-openrc.sh on {})",
        topology.vip()?,
        topology.deployer
    );
    Ok(())
}

async fn prepare(deployer: &Sandbox, topology: &Topology) -> Result<()> {
    let targets = try_join_all(topology.targets.iter().map(|h| farm::connect(&h.name))).await?;
    try_join_all(
        std::iter::once(deployer)
            .chain(&targets)
            .map(farm::wait_until_booted),
    )
    .await?;

    let key = run(
        deployer,
        "test -f ~/.ssh/id_ed25519 || ssh-keygen -q -t ed25519 -N '' -f ~/.ssh/id_ed25519; cat ~/.ssh/id_ed25519.pub",
    )
    .await?;
    let key = key.trim();
    let authorize = format!(
        // The leading newline guards against an existing file that lacks a trailing one.
        "grep -qxF '{key}' ~/.ssh/authorized_keys || printf '\\n%s\\n' '{key}' >> ~/.ssh/authorized_keys"
    );
    try_join_all(targets.iter().map(|t| run(t, &authorize))).await?;
    println!("deployer key authorized on {} target(s)", targets.len());
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
if [ ! -x {OSC_VENV}/bin/openstack ]; then
  python3 -m venv {OSC_VENV}
  {OSC_VENV}/bin/pip install -q -U pip
  {OSC_VENV}/bin/pip install -q -c https://releases.openstack.org/constraints/upper/{OPENSTACK_RELEASE} python-openstackclient
fi
kolla-ansible --version"#
    )
}

async fn configure(deployer: &Sandbox, topology: &Topology) -> Result<()> {
    run(
        deployer,
        &format!(
            "mkdir -p {CONFIG_DIR} && \
             cp -n {VENV}/share/kolla-ansible/etc_examples/kolla/passwords.yml {CONFIG_DIR}/passwords.yml && \
             {VENV}/bin/kolla-genpwd -p {CONFIG_DIR}/passwords.yml"
        ),
    )
    .await?;

    let arch = run(deployer, "uname -m").await?;
    let template = deployer
        .fs()
        .read_to_string(&format!(
            "{VENV}/share/kolla-ansible/ansible/inventory/multinode"
        ))
        .await?;
    let fs = deployer.fs();
    fs.write(
        &format!("{CONFIG_DIR}/globals.yml"),
        config::globals(topology.vip()?, arch.trim()),
    )
    .await?;
    fs.write(INVENTORY, config::inventory(&template, topology))
        .await?;
    fs.write(
        "/root/.ansible.cfg",
        "[defaults]\nhost_key_checking = False\nforks = 20\n\n[ssh_connection]\npipelining = True\n",
    )
    .await?;
    println!(
        "wrote {CONFIG_DIR}/globals.yml and {INVENTORY} (vip {})",
        topology.vip()?
    );
    Ok(())
}

fn kolla_ansible(command: &str) -> String {
    // kolla-ansible shells out to ansible-playbook and ansible-galaxy from PATH.
    format!("PATH={VENV}/bin:$PATH kolla-ansible {command} -i {INVENTORY}")
}

fn verify_script() -> String {
    format!(
        r#"set -euo pipefail
. {CONFIG_DIR}/admin-openrc.sh
os={OSC_VENV}/bin/openstack
$os token issue -f value -c expires
$os endpoint list --service glance -f value -c interface -c url
head -c 1048576 /dev/urandom > /tmp/farm-verify.img
$os image create --disk-format raw --container-format bare --file /tmp/farm-verify.img farm-verify -f value -c id
test "$($os image show farm-verify -f value -c status)" = active
$os image delete farm-verify
rm -f /tmp/farm-verify.img
echo "keystone and glance OK""#
    )
}
