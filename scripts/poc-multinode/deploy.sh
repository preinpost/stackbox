#!/bin/bash
# Runs on the control VM after install.sh: configure Kolla for mn-ctl + mn-c1 + mn-c2 and deploy.
#   deploy.sh [FROM [TO]]   phases: configure bootstrap prechecks deploy post-deploy
# Mirrors configure() and post_deploy_script() in src/kolla/mod.rs, with a multinode inventory.
set -euo pipefail
VENV=/opt/kolla
export PATH=$VENV/bin:$PATH
os=/opt/osc/bin/openstack
from=${1:-configure}
to=${2:-post-deploy}
phases="configure bootstrap prechecks deploy post-deploy"
for p in "$from" "$to"; do
  case " $phases " in *" $p "*) ;; *) echo "unknown phase $p" >&2; exit 2 ;; esac
done
run=''
log() { echo "==> $* ($(date +%T))"; }
ka() { kolla-ansible "$1" -i /etc/kolla/inventory > "/root/$1.log" 2>&1 || { tail -40 "/root/$1.log"; exit 1; }; tail -3 "/root/$1.log"; }

for phase in $phases; do
  [ "$phase" = "$from" ] && run=1
  [ -n "$run" ] || continue
  log "$phase"
  case $phase in
  configure)
    mkdir -p /etc/kolla/config/neutron
    cp -n $VENV/share/kolla-ansible/etc_examples/kolla/passwords.yml /etc/kolla/passwords.yml
    kolla-genpwd -p /etc/kolla/passwords.yml
    suffix=''; [ "$(uname -m)" = aarch64 ] && suffix=-aarch64
    cat > /etc/kolla/globals.yml <<EOF
---
kolla_base_distro: "rocky"
openstack_release: "2026.2"
openstack_tag_suffix: "$suffix"
# One control VM and no HAProxy: the "VIP" is mn-ctl's farm0 address.
kolla_internal_vip_address: "10.254.0.10"
kolla_external_vip_address: "10.254.0.10"
database_address: "10.254.0.10"
network_interface: "farm0"
api_interface: "farm0"
tunnel_interface: "farm0"
kolla_external_vip_interface: "farm0"

kolla_test_images: true
prechecks_enable_host_ntp_checks: false
enable_keepalived: false
enable_haproxy: false
enable_proxysql: false
enable_fluentd: false
openstack_service_workers: 1
openstack_service_rpc_workers: 1
memcached_max_memory: 64
rabbitmq_server_additional_erl_args: "+S 1:1 +sbwt none +sbwtdcpu none +sbwtdio none"

enable_openstack_core: false
enable_mariadb: true
enable_memcached: true
enable_rabbitmq: true
enable_keystone: true
enable_glance: true
glance_backend_file: true
enable_placement: true
enable_horizon: false
enable_neutron: true
enable_nova: true

neutron_plugin_agent: "openvswitch"
ovs_datapath: "netdev"
# Tenant networks are VLANs on farm1 (physnet1, br-tenant) on every node. VXLAN in the
# userspace datapath would need the tunnel address on an OVS bridge.
neutron_tenant_network_types: "vlan"
enable_neutron_provider_networks: true
openvswitch_vswitchd_extra_volumes: "{{ default_extra_volumes + ['/etc/farm/ovs-extend-start:/usr/local/bin/kolla_openvswitch_extend_start:ro'] }}"
neutron_l3_agent_extra_volumes: "{{ neutron_extra_volumes + ['/etc/farm/neutron-sitecustomize.py:/var/lib/kolla/venv/lib64/python3.12/site-packages/sitecustomize.py:ro'] }}"

nova_compute_virt_type: "qemu"
nova_libvirt_cpu_mode: "none"
nova_console: "none"
EOF
    if [ -c /dev/kvm ]; then
      sed -i 's/^nova_compute_virt_type: .*/nova_compute_virt_type: "kvm"/; s/^nova_libvirt_cpu_mode: .*/nova_libvirt_cpu_mode: "host-passthrough"/' /etc/kolla/globals.yml
    fi
    cat > /etc/kolla/config/neutron/ml2_conf.ini <<'EOF'
[ml2_type_vlan]
network_vlan_ranges = physnet1:100:199
[ml2_type_flat]
flat_networks = physnet2
EOF
    cat > /etc/kolla/config/neutron/openvswitch_agent.ini <<'EOF'
[agent]
tunnel_types =
l2_population = false
[securitygroup]
firewall_driver = openvswitch
EOF
    printf '[mysqld]\ninnodb_buffer_pool_size = 512M\nmax_connections = 256\nkey_buffer_size = 16M\nmax_heap_table_size = 16M\ntmp_table_size = 16M\nwsrep_slave_threads = 1\n' > /etc/kolla/config/galera.cnf
    python3 - <<'PY'
import re
from pathlib import Path
text = Path("/opt/kolla/share/kolla-ansible/ansible/inventory/multinode").read_text()
hosts = {
    "control": "mn-ctl ansible_connection=local neutron_external_interface=farm1,farm-ext neutron_bridge_name=br-tenant,br-ex",
    "network": "mn-ctl",
    "compute": "mn-c1 ansible_host=10.254.0.11 neutron_external_interface=farm1 neutron_bridge_name=br-tenant\n"
               "mn-c2 ansible_host=10.254.0.12 neutron_external_interface=farm1 neutron_bridge_name=br-tenant",
    "monitoring": "mn-ctl",
    "storage": "mn-ctl",
}
for group, body in hosts.items():
    text, n = re.subn(rf"^\[{group}\]\n(?:.*\n)*?\n", f"[{group}]\n{body}\n\n", text, count=1, flags=re.M)
    assert n == 1, group
Path("/etc/kolla/inventory").write_text(text)
PY
    printf '[defaults]\nhost_key_checking = False\nforks = 3\n\n[ssh_connection]\npipelining = True\n' > /root/.ansible.cfg
    sed -n '1,/^\[deployment\]/p' /etc/kolla/inventory | grep -v '^#' | grep -v '^$'
    ;;
  bootstrap) ka bootstrap-servers ;;
  prechecks) ka prechecks ;;
  deploy) ka deploy ;;
  post-deploy)
    ka post-deploy
    . /etc/kolla/admin-openrc.sh
    $os network show public >/dev/null 2>&1 ||
      $os network create --external --share --provider-network-type flat --provider-physical-network physnet2 public
    $os subnet show public-subnet >/dev/null 2>&1 ||
      $os subnet create --network public --subnet-range 172.24.4.0/24 --gateway 172.24.4.1 \
        --allocation-pool start=172.24.4.10,end=172.24.4.250 --no-dhcp public-subnet
    $os compute service list
    $os network agent list
    ;;
  esac
  [ "$phase" = "$to" ] && break
done
log done
