#!/bin/bash
# Runs on the control VM: kolla-ansible, its guest-kernel patches and the OpenStack client.
# A copy of install_script() in src/kolla/mod.rs.
set -euo pipefail
VENV=/opt/kolla
OSC_VENV=/opt/osc
RELEASE=2026.2
export PATH=$VENV/bin:$PATH
dnf install -y -q gcc python3-devel libffi-devel openssl-devel
if [ ! -x $VENV/bin/kolla-ansible ]; then
  python3 -m venv $VENV
  $VENV/bin/pip install -q -U pip
  $VENV/bin/pip install -q "git+https://opendev.org/openstack/kolla-ansible@stable/$RELEASE"
fi
kolla-ansible install-deps
python3 - <<'PY'
from pathlib import Path
# Kolla creates the bridges without a datapath type; with the netdev datapath they stay unusable.
path = Path("/opt/kolla/share/kolla-ansible/ansible/roles/openvswitch/tasks/post-config.yml")
text = path.read_text()
marker = "      fail_mode: standalone\n  loop: \"{{ neutron_bridge_name.split(',') }}\"\n"
patched = "      fail_mode: standalone\n      set: \"Bridge {{ item }} datapath_type={{ ovs_datapath }}\"\n  loop: \"{{ neutron_bridge_name.split(',') }}\"\n"
if patched not in text:
    if text.count(marker) != 1:
        raise RuntimeError("unexpected Kolla OVS bridge task; review the datapath_type patch")
    path.write_text(text.replace(marker, patched, 1))
# The guest kernel has no openvswitch module.
path = Path("/opt/kolla/share/kolla-ansible/ansible/roles/openvswitch/tasks/config-host.yml")
text = path.read_text()
marker = "- name: Load and persist openvswitch module\n"
guarded = marker + "  when: ovs_datapath == 'system'\n"
if guarded not in text:
    if text.count(marker) != 1:
        raise RuntimeError("unexpected Kolla openvswitch task; review the netdev datapath patch")
    path.write_text(text.replace(marker, guarded, 1))
# br_netfilter is built in and there is no modprobe.
path = Path("/opt/kolla/share/kolla-ansible/ansible/roles/nova-cell/tasks/config-host.yml")
text = path.read_text()
marker = "  when:\n    - inventory_hostname in groups[nova_cell_compute_group]\n"
patched = "  when:\n    - false\n    - inventory_hostname in groups[nova_cell_compute_group]\n"
if patched not in text:
    if text.count(marker) < 1:
        raise RuntimeError("unexpected Kolla nova-cell when clause; review the br_netfilter patch")
    path.write_text(text.replace(marker, patched, 1))
PY
if [ ! -x $OSC_VENV/bin/openstack ]; then
  python3 -m venv $OSC_VENV
  $OSC_VENV/bin/pip install -q -U pip
  $OSC_VENV/bin/pip install -q -c https://releases.openstack.org/constraints/upper/$RELEASE python-openstackclient
fi
[ -f /root/.ssh/id_ed25519 ] || ssh-keygen -q -t ed25519 -N '' -f /root/.ssh/id_ed25519
kolla-ansible --version
