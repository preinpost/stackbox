set -euo pipefail
# Golden images install the build tools; farms made from the former Docker-built image lack them.
if ! command -v gcc >/dev/null; then
  step "빌드 도구 설치 (gcc, python3-devel, libffi, openssl)"
  dnf install -y -q gcc python3-devel libffi-devel openssl-devel
fi
# One venv per release: the branches differ, so a different release reinstalls it.
# /opt/kolla/bin joins PATH only after the interpreter is resolved. On a retry that directory
# still holds the previous venv's python, which `command -v` would then return as a bare name:
# the stamp would differ from the absolute path the first run recorded, and the script would
# delete the venv and lose the interpreter with it.
python=python3
if ! command -v "$python" >/dev/null; then
  for candidate in /opt/python3/bin/python3 /usr/local/bin/python3; do
    if [ -x "$candidate" ]; then python=$candidate; break; fi
  done
fi
if ! command -v "$python" >/dev/null; then
  echo "this VM has no python3; install it in the golden image and start the farm again" >&2
  exit 1
fi
stamp="2026.2 stable/2026.2 $python"
if [ ! -x /opt/kolla/bin/kolla-ansible ] || [ "$(cat /opt/kolla/.farm-release 2>/dev/null || true)" != "$stamp" ]; then
  step "kolla-ansible 가상환경 생성 (/opt/kolla, $python)"
  rm -rf /opt/kolla
  "$python" -m venv /opt/kolla
  /opt/kolla/bin/pip install -q -U pip
  step "kolla-ansible 설치 (stable/2026.2)"
  /opt/kolla/bin/pip install -q "git+https://opendev.org/openstack/kolla-ansible@stable/2026.2"
  printf '%s' "$stamp" > /opt/kolla/.farm-release
fi
export PATH=/opt/kolla/bin:$PATH
step "kolla-ansible install-deps (컨테이너 런타임 의존성)"
kolla-ansible install-deps
step "게스트 커널 호환 패치 적용 (OVS netdev, br_netfilter)"
# Retain the guest-kernel compatibility patches for Neutron's userspace OVS.
python3 - <<'PY'
from pathlib import Path
# Kolla creates br-ex without a datapath type, so with the netdev datapath it stays unusable
# ("No datapath_id on bridge br-ex"). `set` only applies when the bridge is first created.
path = Path("/opt/kolla/share/kolla-ansible/ansible/roles/openvswitch/tasks/post-config.yml")
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
path = Path("/opt/kolla/share/kolla-ansible/ansible/roles/nova-cell/tasks/precheck.yml")
text = path.read_text()
marker = "  when:\n    - groups[nova_cell_compute_group] | length < 1\n    - not nova_compute_ironic.enabled | bool\n"
patched = marker + "    - not (farm_no_computes | default(false))\n"
if patched not in text:
    if text.count(marker) != 1:
        raise RuntimeError("unexpected Kolla nova-cell precheck; review the no-compute patch")
    path.write_text(text.replace(marker, patched, 1))
# The same kernel has no openvswitch module; farms run OVS with the userspace (netdev) datapath.
path = Path("/opt/kolla/share/kolla-ansible/ansible/roles/openvswitch/tasks/config-host.yml")
text = path.read_text()
marker = "- name: Load and persist openvswitch module\n"
guarded = marker + "  when: ovs_datapath == 'system'\n"
if guarded not in text:
    if text.count(marker) != 1:
        raise RuntimeError("unexpected Kolla openvswitch task; review the netdev datapath patch")
    path.write_text(text.replace(marker, guarded, 1))
# Nova loads br_netfilter with modprobe. This kernel has that bridge filter built in and no modprobe.
path = Path("/opt/kolla/share/kolla-ansible/ansible/roles/nova-cell/tasks/config-host.yml")
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
if [ ! -x /opt/osc/bin/openstack ] || [ "$(cat /opt/osc/.farm-release 2>/dev/null || true)" != "$stamp" ]; then
  step "OpenStack CLI 가상환경 생성 (/opt/osc)"
  rm -rf /opt/osc
  "$python" -m venv /opt/osc
  /opt/osc/bin/pip install -q -U pip
  step "python-openstackclient 설치"
  /opt/osc/bin/pip install -q -c https://releases.openstack.org/constraints/upper/2026.2 python-openstackclient
  printf '%s' "$stamp" > /opt/osc/.farm-release
fi
step "kolla-ansible 버전 확인"
kolla-ansible --version