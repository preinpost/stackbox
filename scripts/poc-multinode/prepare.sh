#!/bin/bash
# Runs on every node before Kolla: the parts of the farm's prepare phase (src/kolla/mod.rs)
# that a node needs. `prepare.sh ctl` also routes Neutron's external network out of eth0.
set -euo pipefail
role=$1
mkdir -p /etc/systemd/system/docker.service.d
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
# Every node runs OVS. Kolla's vswitchd start hook loads the openvswitch module, which the
# guest kernel lacks; the userspace datapath needs none.
cat > /etc/farm/ovs-extend-start <<'EOF'
#!/bin/bash
# Written by farm: kolla's version also runs `modprobe openvswitch`.
if [[ ! -r "/var/log/kolla/openvswitch/ovs-vswitchd.log" ]]; then
    touch /var/log/kolla/openvswitch/ovs-vswitchd.log
    chown openvswitch:openvswitch /var/log/kolla/openvswitch/ovs-vswitchd.log
fi
EOF
chmod 755 /etc/farm/ovs-extend-start
systemctl daemon-reload
if [ "$role" = ctl ]; then
  # Ansible runs mn-ctl's own tasks with its venv python, where bootstrap puts the docker SDK.
  # Tasks the computes delegate to mn-ctl (nova-cell's hypervisor and cell lists) use
  # /usr/bin/python3, and kolla_container imports both docker and dbus.
  rpm -q python3-dbus >/dev/null || dnf install -y -q python3-dbus
  /usr/bin/python3 -c 'import docker' 2>/dev/null || /usr/bin/python3 -m pip install -q docker
  printf 'EXTERNAL_NETWORK=172.24.4.0/24\nEXTERNAL_GATEWAY=172.24.4.1/24\n' > /etc/farm/external.conf
  systemctl restart farm-external
fi
